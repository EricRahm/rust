use rustc::ty::layout::{HasTyCtxt, LayoutOf, TyLayout};
use rustc::ty::Ty;

use super::write::WriteBackendMethods;
use super::CodegenObject;
use rustc::middle::allocator::AllocatorKind;
use rustc::middle::cstore::EncodedMetadata;
use rustc::session::{Session, config};
use rustc::ty::TyCtxt;
use rustc_codegen_utils::codegen_backend::CodegenBackend;
use std::sync::Arc;
use syntax_pos::symbol::InternedString;

pub trait BackendTypes {
    type Value: CodegenObject;
    type BasicBlock: Copy;
    type Type: CodegenObject;
    type Funclet;

    type DIScope: Copy;
}

pub trait Backend<'tcx>:
    Sized + BackendTypes + HasTyCtxt<'tcx> + LayoutOf<Ty = Ty<'tcx>, TyLayout = TyLayout<'tcx>>
{
}

impl<'tcx, T> Backend<'tcx> for T where
    Self: BackendTypes + HasTyCtxt<'tcx> + LayoutOf<Ty = Ty<'tcx>, TyLayout = TyLayout<'tcx>>
{
}

pub trait ExtraBackendMethods: CodegenBackend + WriteBackendMethods + Sized + Send {
    fn new_metadata(&self, sess: TyCtxt<'_, '_>, mod_name: &str) -> Self::Module;
    fn write_compressed_metadata<'gcx>(
        &self,
        tcx: TyCtxt<'gcx, 'gcx>,
        metadata: &EncodedMetadata,
        llvm_module: &mut Self::Module,
    );
    fn codegen_allocator<'gcx>(
        &self,
        tcx: TyCtxt<'gcx, 'gcx>,
        mods: &mut Self::Module,
        kind: AllocatorKind,
    );
    fn compile_codegen_unit<'a, 'tcx: 'a>(&self, tcx: TyCtxt<'tcx, 'tcx>, cgu_name: InternedString);
    // If find_features is true this won't access `sess.crate_types` by assuming
    // that `is_pie_binary` is false. When we discover LLVM target features
    // `sess.crate_types` is uninitialized so we cannot access it.
    fn target_machine_factory(
        &self,
        sess: &Session,
        opt_level: config::OptLevel,
        find_features: bool,
    ) -> Arc<dyn Fn() -> Result<Self::TargetMachine, String> + Send + Sync>;
    fn target_cpu<'b>(&self, sess: &'b Session) -> &'b str;
}
