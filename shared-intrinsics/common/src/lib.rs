//! This crate is used in all three drivers: the bootstrap driver,
//! the host driver, and the runtime driver. This provides a driver
//! agnostic interface for implementing custom Rust intrinsics and
//! translating our `KernelId`s into Rust's `DefId`s.

#![feature(rustc_private, rustc_diagnostic_macros)]
#![feature(core_intrinsics, std_internals)]
#![feature(box_patterns)]
#![feature(link_llvm_intrinsics)]
#![feature(intrinsics)]

#[macro_use]
extern crate rustc;
extern crate rustc_driver;
extern crate rustc_errors;
extern crate rustc_metadata;
extern crate rustc_mir;
extern crate rustc_codegen_utils;
extern crate rustc_data_structures;
extern crate rustc_target;
extern crate serialize;
extern crate syntax;
extern crate syntax_pos;

#[macro_use]
extern crate log;
extern crate num_traits;
extern crate seahash;

extern crate geobacter_core;
extern crate rustc_intrinsics;

pub mod attrs;
pub mod collector;
pub mod hash;
pub mod platform;
pub mod stubbing;

// Note: don't try to depend on `geobacter_std`.
use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;

use geobacter_core::kernel::{KernelInstance};

use self::rustc::hir::def_id::{DefId, CrateNum, };
use self::rustc::middle::lang_items::{self, LangItem, };
use self::rustc::mir::{self, CustomIntrinsicMirGen, Operand, Rvalue,
                       AggregateKind, LocalDecl, Place, StatementKind,
                       Constant, Statement, };
use self::rustc::mir::interpret::{ConstValue, Scalar, Allocation, };
use self::rustc::ty::{self, TyCtxt, layout::Size, Instance, Const, };
use self::rustc_data_structures::indexed_vec::Idx;
use self::rustc_data_structures::sync::{Lrc, };
use crate::serialize::Decodable;
use self::syntax_pos::{Span, DUMMY_SP, };

use crate::rustc_intrinsics::{help::*, codec::*, };

use crate::hash::HashMap;

pub use rustc_intrinsics::*;

pub type CNums = HashMap<(Cow<'static, str>, u64, u64), CrateNum>;

pub trait DefIdFromKernelId {
  fn get_cstore(&self) -> &rustc_metadata::cstore::CStore;
  fn cnum_map(&self) -> Option<&CNums> {
    None
  }

  fn convert_kernel_instance<'tcx>(&self, tcx: TyCtxt<'tcx>,
                                   instance: KernelInstance)
    -> Option<Instance<'tcx>>
  {
    trace!("converting kernel instance for {}", instance.name().unwrap());

    // now decode the substs into `tcx`.
    let mut alloc_state = None;
    let mut decoder = GeobacterDecoder::new(tcx, instance.instance,
                                            &mut alloc_state);

    Instance::decode(&mut decoder).ok()
  }
}
impl DefIdFromKernelId for rustc_metadata::cstore::CStore {
  fn get_cstore(&self) -> &rustc_metadata::cstore::CStore { self }
}
pub trait GetDefIdFromKernelId {
  fn with_self<'tcx, F, R>(tcx: TyCtxt<'tcx>, f: F) -> R
    where F: FnOnce(&dyn DefIdFromKernelId) -> R;
}

pub trait GeobacterCustomIntrinsicMirGen: Send + Sync + 'static {
  fn mirgen_simple_intrinsic<'tcx>(&self,
                                   _stubs: &stubbing::Stubber,
                                   kid_did: &dyn DefIdFromKernelId,
                                   tcx: TyCtxt<'tcx>,
                                   instance: ty::Instance<'tcx>,
                                   mir: &mut mir::Body<'tcx>);

  fn generic_parameter_count<'tcx>(&self, tcx: TyCtxt<'tcx>) -> usize;
  /// The types of the input args.
  fn inputs<'tcx>(&self, tcx: TyCtxt<'tcx>) -> &'tcx ty::List<ty::Ty<'tcx>>;
  /// The return type.
  fn output<'tcx>(&self, tcx: TyCtxt<'tcx>) -> ty::Ty<'tcx>;
}

/// CurrentPlatform doesn't need anything special, but is used from the runtimes.
impl GeobacterCustomIntrinsicMirGen for CurrentPlatform {
  fn mirgen_simple_intrinsic<'tcx>(&self,
                                   _stubs: &stubbing::Stubber,
                                   _kid_did: &dyn DefIdFromKernelId,
                                   tcx: TyCtxt<'tcx>,
                                   instance: ty::Instance<'tcx>,
                                   mir: &mut mir::Body<'tcx>)
  {
    CustomIntrinsicMirGen::mirgen_simple_intrinsic(self, tcx, instance, mir)
  }

  fn generic_parameter_count(&self, tcx: TyCtxt) -> usize {
    CustomIntrinsicMirGen::generic_parameter_count(self,  tcx)
  }
  /// The types of the input args.
  fn inputs<'tcx>(&self, tcx: TyCtxt<'tcx>) -> &'tcx ty::List<ty::Ty<'tcx>> {
    CustomIntrinsicMirGen::inputs(self,  tcx)
  }
  /// The return type.
  fn output<'tcx>(&self, tcx: TyCtxt<'tcx>) -> ty::Ty<'tcx> {
    CustomIntrinsicMirGen::output(self,  tcx)
  }
}

pub struct GeobacterMirGen<T, U>(T, PhantomData<U>)
  where T: GeobacterCustomIntrinsicMirGen + Send + Sync + 'static,
        U: GetDefIdFromKernelId + Send + Sync + 'static;

impl<T, U> GeobacterMirGen<T, U>
  where T: GeobacterCustomIntrinsicMirGen + fmt::Display + Send + Sync + 'static,
        U: GetDefIdFromKernelId + Send + Sync + 'static,
{
  pub fn new(intrinsic: T, _: &U) -> (String, Lrc<dyn CustomIntrinsicMirGen>) {
    let name = format!("{}", intrinsic);
    let mirgen: Self = GeobacterMirGen(intrinsic, PhantomData);
    let mirgen = Lrc::new(mirgen) as Lrc<_>;
    (name, mirgen)
  }
}
impl<T, U> GeobacterMirGen<T, U>
  where T: GeobacterCustomIntrinsicMirGen + Send + Sync + 'static,
        U: GetDefIdFromKernelId + Send + Sync + 'static,
{
  pub fn wrap(intrinsic: T, _: &U) -> Lrc<dyn CustomIntrinsicMirGen> {
    let mirgen: Self = GeobacterMirGen(intrinsic, PhantomData);
    let mirgen = Lrc::new(mirgen) as Lrc<_>;
    mirgen
  }
}

impl<T, U> CustomIntrinsicMirGen for GeobacterMirGen<T, U>
  where T: GeobacterCustomIntrinsicMirGen + Send + Sync + 'static,
        U: GetDefIdFromKernelId + Send + Sync,
{
  fn mirgen_simple_intrinsic<'tcx>(&self,
                                   tcx: TyCtxt<'tcx>,
                                   instance: ty::Instance<'tcx>,
                                   mir: &mut mir::Body<'tcx>)
  {
    U::with_self(tcx, |s| {
      let stubs = stubbing::Stubber::default(); // TODO move into the drivers
      self.0.mirgen_simple_intrinsic(&stubs, s, tcx,
                                     instance, mir)
    })
  }

  fn generic_parameter_count(&self, tcx: TyCtxt) -> usize {
    self.0.generic_parameter_count(tcx)
  }
  /// The types of the input args.
  fn inputs<'tcx>(&self, tcx: TyCtxt<'tcx>) -> &'tcx ty::List<ty::Ty<'tcx>> {
    self.0.inputs(tcx)
  }
  /// The return type.
  fn output<'tcx>(&self, tcx: TyCtxt<'tcx>) -> ty::Ty<'tcx> {
    self.0.output(tcx)
  }
}

/// Either call the instance returned from `f` or insert code to panic.
/// TODO this should probably be turned into an attribute so it's more systematic.
pub fn redirect_or_panic<'tcx, F>(tcx: TyCtxt<'tcx>,
                                  mir: &mut mir::Body<'tcx>,
                                  f: F)
  where F: FnOnce() -> Option<Instance<'tcx>>,
{
  pub fn langcall(tcx: TyCtxt,
                  span: Option<Span>,
                  msg: &str,
                  li: LangItem)
    -> DefId
  {
    tcx.lang_items().require(li).unwrap_or_else(|s| {
      let msg = format!("{} {}", msg, s);
      match span {
        Some(span) => tcx.sess.span_fatal(span, &msg[..]),
        None => tcx.sess.fatal(&msg[..]),
      }
    })
  }

  fn static_str_operand<'tcx, T>(tcx: TyCtxt<'tcx>,
                                 source_info: mir::SourceInfo,
                                 str: T) -> Operand<'tcx>
    where T: fmt::Display,
  {
    let str = format!("{}", str);
    let alloc = Allocation::from_byte_aligned_bytes(str.as_bytes());
    let v = ConstValue::Slice {
      data: tcx.intern_const_alloc(alloc),
      start: 0,
      end: str.len(),
    };
    let v = tcx.mk_const(Const {
      ty: tcx.mk_static_str(),
      val: v,
    });
    let v = Constant {
      span: source_info.span,
      literal: v,
      user_ty: None,
    };
    let v = Box::new(v);
    Operand::Constant(v)
  }

  let source_info = mir::SourceInfo {
    span: DUMMY_SP,
    scope: mir::OUTERMOST_SOURCE_SCOPE,
  };

  let mk_u32 = |v: u32| {
    let v = Scalar::from_uint(v, Size::from_bytes(4));
    let v = ConstValue::Scalar(v);
    let v = tcx.mk_const(Const {
      ty: tcx.types.u32,
      val: v,
    });
    let v = Constant {
      span: source_info.span,
      literal: v,
      user_ty: None,
    };
    let v = Box::new(v);
    Operand::Constant(v)
  };

  let mut bb = mir::BasicBlockData {
    statements: Vec::new(),
    terminator: Some(mir::Terminator {
      source_info: source_info.clone(),
      kind: mir::TerminatorKind::Return,
    }),

    is_cleanup: false,
  };

  let (real_instance, args, term_kind) = match f() {
    Some(instance) => {
      (instance, vec![], mir::TerminatorKind::Return)
    },
    None => {
      // call `panic` from `libcore`
      // `fn panic(expr_file_line_col: &(&'static str, &'static str, u32, u32)) -> !`
      let lang_item = lang_items::PanicFnLangItem;

      let expr = static_str_operand(tcx, source_info.clone(),
                                    "TODO panic expr");
      let file = static_str_operand(tcx, source_info.clone(),
                                    "TODO panic file");
      let line = mk_u32(0); // TODO
      let col  = mk_u32(0); // TODO
      let rvalue = Rvalue::Aggregate(Box::new(AggregateKind::Tuple),
                                     vec![expr, file, line, col]);
      let arg_ty = tcx.intern_tup(&[
        tcx.mk_static_str(),
        tcx.mk_static_str(),
        tcx.types.u32,
        tcx.types.u32,
      ]);
      let arg_local = LocalDecl::new_temp(arg_ty, DUMMY_SP);
      let arg_local_id = Place::from(mir.local_decls.next_index());
      mir.local_decls.push(arg_local);
      let stmt_kind = StatementKind::Assign(arg_local_id.clone(),
                                            Box::new(rvalue));
      let stmt = Statement {
        source_info: source_info.clone(),
        kind: stmt_kind,
      };
      bb.statements.push(stmt);

      let arg_ref_ty = tcx.mk_imm_ref(tcx.lifetimes.re_erased, arg_ty);
      let arg_ref_local = LocalDecl::new_temp(arg_ref_ty, DUMMY_SP);
      let arg_ref_local_id = Place::from(mir.local_decls.next_index());
      mir.local_decls.push(arg_ref_local);
      let rvalue = Rvalue::Ref(tcx.lifetimes.re_erased,
                               mir::BorrowKind::Shared,
                               arg_local_id);
      let stmt_kind = StatementKind::Assign(arg_ref_local_id.clone(),
                                            Box::new(rvalue));
      let stmt = Statement {
        source_info: source_info.clone(),
        kind: stmt_kind,
      };
      bb.statements.push(stmt);

      let def_id = langcall(tcx, None, "", lang_item);
      let instance = Instance::mono(tcx, def_id);

      (instance,
       vec![Operand::Copy(arg_ref_local_id), ],
       mir::TerminatorKind::Unreachable)
    },
  };
  debug!("mirgen intrinsic into {}", real_instance);
  let success = mir::BasicBlock::new(mir.basic_blocks().next_index().index() + 1);
  let fn_ty = real_instance.ty(tcx);
  bb.terminator.as_mut()
    .unwrap()
    .kind = mir::TerminatorKind::Call {
    func: tcx.mk_const_op(source_info.clone(),
                          *ty::Const::zero_sized(tcx, fn_ty)),
    args,
    destination: Some((Place::RETURN_PLACE.clone(), success)),
    cleanup: None,
    from_hir_call: false,
  };
  mir.basic_blocks_mut().push(bb);
  let bb = mir::BasicBlockData {
    statements: Vec::new(),
    terminator: Some(mir::Terminator {
      source_info: source_info.clone(),
      kind: term_kind,
    }),

    is_cleanup: false,
  };
  mir.basic_blocks_mut().push(bb);
}

pub trait PlatformImplDetail: Send + Sync + 'static {
  fn kernel_instance() -> KernelInstance;
}

/// Kill (ie `abort()`) the current workitem/thread only.
pub struct WorkItemKill<T>(PhantomData<T>)
  where T: PlatformImplDetail;
impl<T> WorkItemKill<T>
  where T: PlatformImplDetail,
{
  fn kernel_instance(&self) -> KernelInstance {
    T::kernel_instance()
  }
}
impl<T> Default for WorkItemKill<T>
  where T: PlatformImplDetail,
{
  fn default() -> Self {
    WorkItemKill(PhantomData)
  }
}
impl<T> GeobacterCustomIntrinsicMirGen for WorkItemKill<T>
  where T: PlatformImplDetail,
{
  fn mirgen_simple_intrinsic<'tcx>(&self,
                                   _stubs: &stubbing::Stubber,
                                   kid_did: &dyn DefIdFromKernelId,
                                   tcx: TyCtxt<'tcx>,
                                   _instance: ty::Instance<'tcx>,
                                   mir: &mut mir::Body<'tcx>)
  {
    trace!("mirgen intrinsic {}", self);

    redirect_or_panic(tcx, mir, move || {
      let id = self.kernel_instance();
      let instance = kid_did.convert_kernel_instance(tcx, id)
        .expect("failed to convert kernel id to def id");
      Some(instance)
    });
  }

  fn generic_parameter_count(&self, _tcx: TyCtxt) -> usize {
    0
  }
  /// The types of the input args.
  fn inputs<'tcx>(&self, tcx: TyCtxt<'tcx>)
    -> &'tcx ty::List<ty::Ty<'tcx>>
  {
    tcx.intern_type_list(&[])
  }
  /// The return type.
  fn output<'tcx>(&self, tcx: TyCtxt<'tcx>) -> ty::Ty<'tcx> {
    tcx.types.never
  }
}

impl<T> fmt::Display for WorkItemKill<T>
  where T: PlatformImplDetail,
{
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    write!(f, "__geobacter_kill")
  }
}
pub struct HostKillDetail;
impl PlatformImplDetail for HostKillDetail {
  fn kernel_instance() -> KernelInstance {
    fn host_kill() -> ! {
      panic!("__geobacter_kill");
    }

    KernelInstance::get(&host_kill)
  }
}
pub type WorkItemHostKill = WorkItemKill<HostKillDetail>;
