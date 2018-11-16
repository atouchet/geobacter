
use {is_host, };

use std::alloc::{Layout, };
use std::ffi::c_void;
use std::marker::{PhantomData, };
use std::mem::{size_of, transmute, align_of_val, size_of_val,
               ManuallyDrop, };
use std::ops::{Deref, DerefMut, Index, IndexMut, };
use std::ptr::{NonNull, };
use std::slice::{from_raw_parts, from_raw_parts_mut, SliceIndex, };
use std::sync::{Arc, atomic::AtomicUsize, };

use hsa_rt::ffi;
use hsa_rt::agent::Agent;
use hsa_rt::error::Error;
use hsa_rt::utils::set_data_ptr;

use nd;

pub mod slice;
pub mod ref_;

pub trait AsPtrValue {
  type Target: ?Sized;
  fn byte_len(&self) -> usize;
  fn as_ptr_value(&self) -> NonNull<Self::Target>;

  fn as_u8_slice(&self) -> &[u8] {
    let len = self.byte_len();
    let ptr = self.as_ptr_value().as_ptr() as *mut u8;
    unsafe {
      from_raw_parts(ptr as *const u8, len)
    }
  }
}
impl<T> AsPtrValue for Vec<T> {
  type Target = T;
  fn byte_len(&self) -> usize {
    size_of::<T>() * self.len()
  }
  fn as_ptr_value(&self) -> NonNull<Self::Target> {
    unsafe { NonNull::new_unchecked(self.as_ptr() as *mut _) }
  }
}
impl<'a, T> AsPtrValue for &'a [T] {
  type Target = T;
  fn byte_len(&self) -> usize {
    size_of::<T>() * (*self).len()
  }
  fn as_ptr_value(&self) -> NonNull<Self::Target> {
    unsafe { NonNull::new_unchecked((*self).as_ptr() as *mut _) }
  }
}
impl<T> AsPtrValue for Box<T> {
  type Target = T;
  fn byte_len(&self) -> usize {
    size_of::<T>()
  }
  fn as_ptr_value(&self) -> NonNull<Self::Target> {
    unsafe {
      ::std::mem::transmute_copy(self)
    }
  }
}
impl<T> AsPtrValue for Arc<T>
  where T: ?Sized,
{
  type Target = ArcInner<T>;
  fn byte_len(&self) -> usize {
    let align = align_of_val(&**self);
    let layout = Layout::new::<ArcInner<()>>();
    let offset = layout.size() + layout.padding_needed_for(align);

    let val_size = size_of_val(&**self);
    offset + val_size
  }
  fn as_ptr_value(&self) -> NonNull<Self::Target> {
    let this = self.clone();
    let ptr = Arc::into_raw(this);
    ::std::mem::drop(unsafe { Arc::from_raw(ptr) });

    // adjust the pointer to include the two ref counters.
    // Align the unsized value to the end of the ArcInner.
    // Because it is ?Sized, it will always be the last field in memory.
    let align = align_of_val(&**self);
    let layout = Layout::new::<ArcInner<()>>();
    let offset = (layout.size() + layout.padding_needed_for(align)) as isize;
    // Reverse the offset to find the original ArcInner.
    let fake_ptr = ptr as *mut ArcInner<T>;
    let arc_ptr = unsafe {
      set_data_ptr(fake_ptr,
                   (ptr as *mut u8).offset(-offset))
    };

    unsafe { NonNull::new_unchecked(arc_ptr as *mut Self::Target) }
  }
}
impl<T, D> AsPtrValue for nd::ArrayBase<T, D>
  where T: nd::DataOwned,
        D: nd::Dimension,
{
  type Target = T::Elem;
  fn byte_len(&self) -> usize {
    self.as_slice_memory_order()
      .expect("owned nd::ArrayBase isn't contiguous")
      .len() * size_of::<T::Elem>()
  }
  fn as_ptr_value(&self) -> NonNull<Self::Target> {
    unsafe { NonNull::new_unchecked(self.as_ptr() as *mut _) }
  }
}

enum HostPtr<T>
  where T: AsPtrValue,
{
  Obj(T),
  Ptr(NonNull<T::Target>),
}
impl<T> HostPtr<T>
  where T: AsPtrValue,
{
  fn host_ptr(&self) -> NonNull<T::Target> {
    match self {
      &HostPtr::Obj(ref obj) => obj.as_ptr_value(),
      &HostPtr::Ptr(ptr) => ptr,
    }
  }
  fn host_obj_ref(&self) -> &T {
    match self {
      &HostPtr::Obj(ref obj) => obj,
      _ => unreachable!(),
    }
  }
  fn host_obj_mut(&mut self) -> &mut T {
    match self {
      &mut HostPtr::Obj(ref mut obj) => obj,
      _ => unreachable!(),
    }
  }
  fn take_obj(&mut self) -> T {
    let mut new_val = HostPtr::Ptr(self.host_ptr());
    ::std::mem::swap(&mut new_val, self);
    match new_val {
      HostPtr::Obj(obj) => obj,
      _ => panic!("don't have object anymore"),
    }
  }
}

pub struct HostLockedAgentPtr<T>
  where T: AsPtrValue,
{
  // always `HostPtr::Obj`, except during `Drop`.
  host: HostPtr<T>,
  agent_ptr: NonNull<T::Target>,
}

impl<T> HostLockedAgentPtr<T>
  where T: AsPtrValue,
{
  pub fn host_ptr(&self) -> NonNull<T::Target> {
    self.host.host_ptr()
  }
  pub fn agent_ptr(&self) -> &NonNull<T::Target> { &self.agent_ptr }

  pub fn unlock(mut self) -> T { self.host.take_obj() }
}
impl<T> HostLockedAgentPtr<Vec<T>> {
  pub fn as_slice(&self) -> slice::SliceRef<T> {
    slice::SliceRef {
      _owner: PhantomData,
      host: self.host_ptr(),
      agent: self.agent_ptr,
      len: self.host.host_obj_ref().len(),
    }
  }
  pub fn as_slice_mut(&mut self) -> slice::SliceMut<T> {
    slice::SliceMut {
      _owner: PhantomData,
      host: self.host_ptr(),
      agent: self.agent_ptr,
      len: self.host.host_obj_ref().len(),
    }
  }

  pub fn slice_ref<I>(&self, index: I) -> slice::SliceRef<T>
    where I: SliceIndex<[T], Output = [T]>,
  {
    // compute the offset w/ the host ref, then use that to offset
    // the agent ptr.

    let slice = index.index(self.host.host_obj_ref().as_ref());

    let host_ptr = self.host_ptr().as_ptr() as usize;
    let slice_ptr = slice.as_ptr() as usize;

    let offset = slice_ptr - host_ptr;

    slice::SliceRef {
      _owner: PhantomData,
      host: unsafe { NonNull::new_unchecked(slice.as_ptr() as *mut _) },
      agent: unsafe {
        let p = self.agent_ptr()
          .as_ptr()
          .add(offset);
        NonNull::new_unchecked(p)
      },
      len: slice.len(),
    }
  }
  pub fn slice_mut<I>(&mut self, index: I) -> slice::SliceMut<T>
    where I: SliceIndex<[T], Output = [T]>,
  {
    // compute the offset w/ the host ref, then use that to offset
    // the agent ptr.

    let slice = index.index(self.host.host_obj_ref().as_ref());

    let host_ptr = self.host_ptr().as_ptr() as usize;
    let slice_ptr = slice.as_ptr() as usize;

    let offset = slice_ptr - host_ptr;

    slice::SliceMut {
      _owner: PhantomData,
      host: unsafe { NonNull::new_unchecked(slice.as_ptr() as *mut _) },
      agent: unsafe {
        let p = self.agent_ptr()
          .as_ptr()
          .add(offset);
        NonNull::new_unchecked(p)
      },
      len: slice.len(),
    }
  }
}
impl<T> HostLockedAgentPtr<Box<T>> {
  pub fn as_agent_ref(&self) -> &T {
    unsafe { self.agent_ptr().as_ref() }
  }
  pub fn as_agent_mut(&mut self) -> &mut T {
    unsafe { self.agent_ptr.as_mut() }
  }
}
impl<T> HostLockedAgentPtr<Arc<T>>
  where T: ?Sized + Sync,
{
  /// the agent pointer will point to the start of the `ArcInner<T>`
  /// This function returns an agent pointer which starts at the data.
  fn agent_arc_ptr(&self) -> NonNull<T> {
    // Align the unsized value to the end of the ArcInner.
    // Because it is ?Sized, it will always be the last field in memory.
    let align = align_of_val(&**self);
    let layout = Layout::new::<ArcInner<()>>();
    let offset = (layout.size() + layout.padding_needed_for(align)) as isize;

    // Reverse the offset to find the original ArcInner.
    let fake_ptr = self.agent_ptr()
      .as_ptr() as *mut ArcInner<T>;

    let arc_ptr = unsafe {
      // This might seem like it would always segfault, but
      // `fake_ptr` is actually fat when T is unsized.
      // So we're actually just writing to a location on the stack.
      set_data_ptr(fake_ptr,
                   (fake_ptr as *mut u8).offset(offset))
    };
    unsafe {
      NonNull::new_unchecked(arc_ptr as *mut T)
    }
  }
  pub fn agent_arc(&self) -> Arc<T> {
    // the agent ptr will start at the start of the two ref counters.
    // we need to adjust the pointer to the start of the data, skipping
    // those two counters.
    // upref the arc for the value we're going to return
    unsafe { ::std::mem::forget(self.clone()) }

    let arc_ptr = self.agent_arc_ptr();

    unsafe { Arc::from_raw(arc_ptr.as_ptr()) }
  }

  pub fn as_agent_ref(&self) -> &T {
    let arc_ptr = self.agent_arc_ptr();
    unsafe { transmute(arc_ptr.as_ref()) }
  }
  pub fn as_agent_mut(&mut self) -> &mut T {
    let mut arc_ptr = self.agent_arc_ptr();
    unsafe { transmute(arc_ptr.as_mut()) }
  }
}
impl<T, D> HostLockedAgentPtr<nd::ArrayBase<T, D>>
  where T: nd::DataOwned,
        D: nd::Dimension,
{
  pub fn host_view(&self) -> nd::ArrayView<T::Elem, D> {
    self.host.host_obj_ref().view()
  }
  pub fn host_view_mut(&mut self) -> nd::ArrayViewMut<T::Elem, D>
    where T: nd::DataMut,
  {
    self.host.host_obj_mut().view_mut()
  }
  pub fn agent_view(&self) -> nd::ArrayView<T::Elem, D> {
    unsafe {
      nd::ArrayView::from_shape_ptr(self.host.host_obj_ref().raw_dim(),
                                    self.agent_ptr().as_ptr() as *const _)
    }
  }
  pub fn agent_view_mut(&mut self) -> nd::ArrayViewMut<T::Elem, D> {
    unsafe {
      nd::ArrayViewMut::from_shape_ptr(self.host.host_obj_ref().raw_dim(),
                                       self.agent_ptr().as_ptr())
    }
  }
}
impl<T> Drop for HostLockedAgentPtr<T>
  where T: AsPtrValue,
{
  fn drop(&mut self) {
    let ptr = self.host_ptr().as_ptr();
    info!("unlocking address {:p}", ptr);
    unsafe {
      ffi::hsa_amd_memory_unlock(ptr as *mut c_void);
      // ignore result.
    }
  }
}
impl<T> Deref for HostLockedAgentPtr<T>
  where T: AsPtrValue + Deref,
{
  type Target = <T as Deref>::Target;
  fn deref(&self) -> &<T as Deref>::Target {
    self.host.host_obj_ref()
      .deref()
  }
}
impl<T> DerefMut for HostLockedAgentPtr<T>
  where T: AsPtrValue + DerefMut,
{
  fn deref_mut(&mut self) -> &mut <T as Deref>::Target {
    self.host.host_obj_mut()
      .deref_mut()
  }
}
unsafe impl<T> Send for HostLockedAgentPtr<T>
  where T: AsPtrValue + Send,
{ }
unsafe impl<T> Sync for HostLockedAgentPtr<T>
  where T: AsPtrValue + Sync,
{ }

/// Locks `self` in host memory, and gives access to the specified agents.
/// If `agents` as no elements, access will be given to everyone.
pub trait HostLockedAgentMemory: Sized + AsPtrValue
  where Self::Target: Sized,
{
  fn lock_memory_globally(self) -> Result<HostLockedAgentPtr<Self>, Error> {
    let agents_len = 0;
    let agents_ptr = 0 as *mut _;
    let mut agent_ptr = 0 as *mut u8 as *mut Self::Target;
    {
      let bytes = self.as_u8_slice();
      check_err!(ffi::hsa_amd_memory_lock(bytes.as_ptr() as *mut c_void,
                                          bytes.len(),
                                          agents_ptr,
                                          agents_len,
                                          transmute(&mut agent_ptr)))?;
      assert_ne!(agent_ptr as *mut u8 as usize, 0);
    }

    Ok(HostLockedAgentPtr {
      host: HostPtr::Obj(self),
      agent_ptr: unsafe { NonNull::new_unchecked(agent_ptr) },
    })
  }
  fn lock_memory<'a>(self, agents: &[&Agent])
    -> Result<HostLockedAgentPtr<Self>, Error>
  {
    let mut agents: Vec<_> = agents
      .iter()
      .map(|agent| unsafe { agent.raw_handle() } )
      .collect();

    let agents_len = agents.len();
    let agents_ptr = agents.as_mut_ptr();

    let mut agent_ptr = 0 as *mut u8 as *mut Self::Target;
    {
      let bytes = self.as_u8_slice();
      info!("locking address {:p}-{:p}",
            bytes.as_ptr(),
            unsafe { bytes.as_ptr().offset(bytes.len() as isize) });
      check_err!(ffi::hsa_amd_memory_lock(bytes.as_ptr() as *mut c_void,
                                          bytes.len(),
                                          agents_ptr,
                                          agents_len as _,
                                          transmute(&mut agent_ptr)))?;
      assert_ne!(agent_ptr as *mut u8 as usize, 0);
    }

    Ok(HostLockedAgentPtr {
      host: HostPtr::Obj(self),
      agent_ptr: unsafe { NonNull::new_unchecked(agent_ptr) },
    })
  }
}
impl<T> HostLockedAgentMemory for Vec<T> { }
impl<T> HostLockedAgentMemory for Box<T> { }
impl<T> HostLockedAgentMemory for Arc<T>
  where T: Sized,
{ }
impl<T, D> HostLockedAgentMemory for nd::ArrayBase<T, D>
  where T: nd::DataOwned,
        D: nd::Dimension,
{ }

/// Identical definition to the one in `std`. THIS MUST MATCH THAT.
#[doc(hidden)]
pub struct ArcInner<T>
  where T: ?Sized,
{
  _strong: AtomicUsize,

  // the value usize::MAX acts as a sentinel for temporarily "locking" the
  // ability to upgrade weak pointers or downgrade strong ones; this is used
  // to avoid races in `make_mut` and `get_mut`.
  _weak: AtomicUsize,

  _data: T,
}
