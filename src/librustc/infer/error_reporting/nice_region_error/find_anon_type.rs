use crate::hir;
use crate::ty::{self, Region, TyCtxt};
use crate::hir::Node;
use crate::middle::resolve_lifetime as rl;
use crate::hir::intravisit::{self, NestedVisitorMap, Visitor};
use crate::infer::error_reporting::nice_region_error::NiceRegionError;

impl<'a, 'gcx, 'tcx> NiceRegionError<'a, 'gcx, 'tcx> {
    /// This function calls the `visit_ty` method for the parameters
    /// corresponding to the anonymous regions. The `nested_visitor.found_type`
    /// contains the anonymous type.
    ///
    /// # Arguments
    /// region - the anonymous region corresponding to the anon_anon conflict
    /// br - the bound region corresponding to the above region which is of type `BrAnon(_)`
    ///
    /// # Example
    /// ```
    /// fn foo(x: &mut Vec<&u8>, y: &u8)
    ///    { x.push(y); }
    /// ```
    /// The function returns the nested type corresponding to the anonymous region
    /// for e.g., `&u8` and Vec<`&u8`.
    pub(super) fn find_anon_type(
        &self,
        region: Region<'tcx>,
        br: &ty::BoundRegion,
    ) -> Option<(&hir::Ty, &hir::FnDecl)> {
        if let Some(anon_reg) = self.tcx().is_suitable_region(region) {
            let def_id = anon_reg.def_id;
            if let Some(node_id) = self.tcx().hir().as_local_node_id(def_id) {
                let fndecl = match self.tcx().hir().get(node_id) {
                    Node::Item(&hir::Item {
                        node: hir::ItemKind::Fn(ref fndecl, ..),
                        ..
                    }) => &fndecl,
                    Node::TraitItem(&hir::TraitItem {
                        node: hir::TraitItemKind::Method(ref m, ..),
                        ..
                    })
                    | Node::ImplItem(&hir::ImplItem {
                        node: hir::ImplItemKind::Method(ref m, ..),
                        ..
                    }) => &m.decl,
                    _ => return None,
                };

                return fndecl
                    .inputs
                    .iter()
                    .filter_map(|arg| self.find_component_for_bound_region(arg, br))
                    .next()
                    .map(|ty| (ty, &**fndecl));
            }
        }
        None
    }

    // This method creates a FindNestedTypeVisitor which returns the type corresponding
    // to the anonymous region.
    fn find_component_for_bound_region(
        &self,
        arg: &'gcx hir::Ty,
        br: &ty::BoundRegion,
    ) -> Option<(&'gcx hir::Ty)> {
        let mut nested_visitor = FindNestedTypeVisitor {
            tcx: self.tcx(),
            bound_region: *br,
            found_type: None,
            current_index: ty::INNERMOST,
        };
        nested_visitor.visit_ty(arg);
        nested_visitor.found_type
    }
}

// The FindNestedTypeVisitor captures the corresponding `hir::Ty` of the
// anonymous region. The example above would lead to a conflict between
// the two anonymous lifetimes for &u8 in x and y respectively. This visitor
// would be invoked twice, once for each lifetime, and would
// walk the types like &mut Vec<&u8> and &u8 looking for the HIR
// where that lifetime appears. This allows us to highlight the
// specific part of the type in the error message.
struct FindNestedTypeVisitor<'gcx, 'tcx> {
    tcx: TyCtxt<'gcx, 'tcx>,
    // The bound_region corresponding to the Refree(freeregion)
    // associated with the anonymous region we are looking for.
    bound_region: ty::BoundRegion,
    // The type where the anonymous lifetime appears
    // for e.g., Vec<`&u8`> and <`&u8`>
    found_type: Option<&'gcx hir::Ty>,
    current_index: ty::DebruijnIndex,
}

impl Visitor<'gcx> for FindNestedTypeVisitor<'gcx, 'tcx> {
    fn nested_visit_map<'this>(&'this mut self) -> NestedVisitorMap<'this, 'gcx> {
        NestedVisitorMap::OnlyBodies(&self.tcx.hir())
    }

    fn visit_ty(&mut self, arg: &'gcx hir::Ty) {
        match arg.node {
            hir::TyKind::BareFn(_) => {
                self.current_index.shift_in(1);
                intravisit::walk_ty(self, arg);
                self.current_index.shift_out(1);
                return;
            }

            hir::TyKind::TraitObject(ref bounds, _) => for bound in bounds {
                self.current_index.shift_in(1);
                self.visit_poly_trait_ref(bound, hir::TraitBoundModifier::None);
                self.current_index.shift_out(1);
            },

            hir::TyKind::Rptr(ref lifetime, _) => {
                // the lifetime of the TyRptr
                let hir_id = lifetime.hir_id;
                match (self.tcx.named_region(hir_id), self.bound_region) {
                    // Find the index of the anonymous region that was part of the
                    // error. We will then search the function parameters for a bound
                    // region at the right depth with the same index
                    (
                        Some(rl::Region::LateBoundAnon(debruijn_index, anon_index)),
                        ty::BrAnon(br_index),
                    ) => {
                        debug!(
                            "LateBoundAnon depth = {:?} anon_index = {:?} br_index={:?}",
                            debruijn_index,
                            anon_index,
                            br_index
                        );
                        if debruijn_index == self.current_index && anon_index == br_index {
                            self.found_type = Some(arg);
                            return; // we can stop visiting now
                        }
                    }

                    // Find the index of the named region that was part of the
                    // error. We will then search the function parameters for a bound
                    // region at the right depth with the same index
                    (Some(rl::Region::EarlyBound(_, id, _)), ty::BrNamed(def_id, _)) => {
                        debug!(
                            "EarlyBound self.infcx.tcx.hir().local_def_id(id)={:?} \
                             def_id={:?}",
                            id,
                            def_id
                        );
                        if id == def_id {
                            self.found_type = Some(arg);
                            return; // we can stop visiting now
                        }
                    }

                    // Find the index of the named region that was part of the
                    // error. We will then search the function parameters for a bound
                    // region at the right depth with the same index
                    (
                        Some(rl::Region::LateBound(debruijn_index, id, _)),
                        ty::BrNamed(def_id, _),
                    ) => {
                        debug!(
                            "FindNestedTypeVisitor::visit_ty: LateBound depth = {:?}",
                            debruijn_index
                        );
                        debug!("self.infcx.tcx.hir().local_def_id(id)={:?}", id);
                        debug!("def_id={:?}", def_id);
                        if debruijn_index == self.current_index && id == def_id {
                            self.found_type = Some(arg);
                            return; // we can stop visiting now
                        }
                    }

                    (Some(rl::Region::Static), _)
                    | (Some(rl::Region::Free(_, _)), _)
                    | (Some(rl::Region::EarlyBound(_, _, _)), _)
                    | (Some(rl::Region::LateBound(_, _, _)), _)
                    | (Some(rl::Region::LateBoundAnon(_, _)), _)
                    | (None, _) => {
                        debug!("no arg found");
                    }
                }
            }
            // Checks if it is of type `hir::TyKind::Path` which corresponds to a struct.
            hir::TyKind::Path(_) => {
                let subvisitor = &mut TyPathVisitor {
                    tcx: self.tcx,
                    found_it: false,
                    bound_region: self.bound_region,
                    current_index: self.current_index,
                };
                intravisit::walk_ty(subvisitor, arg); // call walk_ty; as visit_ty is empty,
                                                      // this will visit only outermost type
                if subvisitor.found_it {
                    self.found_type = Some(arg);
                }
            }
            _ => {}
        }
        // walk the embedded contents: e.g., if we are visiting `Vec<&Foo>`,
        // go on to visit `&Foo`
        intravisit::walk_ty(self, arg);
    }
}

// The visitor captures the corresponding `hir::Ty` of the anonymous region
// in the case of structs ie. `hir::TyKind::Path`.
// This visitor would be invoked for each lifetime corresponding to a struct,
// and would walk the types like Vec<Ref> in the above example and Ref looking for the HIR
// where that lifetime appears. This allows us to highlight the
// specific part of the type in the error message.
struct TyPathVisitor<'gcx, 'tcx> {
    tcx: TyCtxt<'gcx, 'tcx>,
    found_it: bool,
    bound_region: ty::BoundRegion,
    current_index: ty::DebruijnIndex,
}

impl Visitor<'gcx> for TyPathVisitor<'gcx, 'tcx> {
    fn nested_visit_map<'this>(&'this mut self) -> NestedVisitorMap<'this, 'gcx> {
        NestedVisitorMap::OnlyBodies(&self.tcx.hir())
    }

    fn visit_lifetime(&mut self, lifetime: &hir::Lifetime) {
        match (self.tcx.named_region(lifetime.hir_id), self.bound_region) {
            // the lifetime of the TyPath!
            (Some(rl::Region::LateBoundAnon(debruijn_index, anon_index)), ty::BrAnon(br_index)) => {
                if debruijn_index == self.current_index && anon_index == br_index {
                    self.found_it = true;
                    return;
                }
            }

            (Some(rl::Region::EarlyBound(_, id, _)), ty::BrNamed(def_id, _)) => {
                debug!(
                    "EarlyBound self.infcx.tcx.hir().local_def_id(id)={:?} \
                     def_id={:?}",
                    id,
                    def_id
                );
                if id == def_id {
                    self.found_it = true;
                    return; // we can stop visiting now
                }
            }

            (Some(rl::Region::LateBound(debruijn_index, id, _)), ty::BrNamed(def_id, _)) => {
                debug!(
                    "FindNestedTypeVisitor::visit_ty: LateBound depth = {:?}",
                    debruijn_index,
                );
                debug!("id={:?}", id);
                debug!("def_id={:?}", def_id);
                if debruijn_index == self.current_index && id == def_id {
                    self.found_it = true;
                    return; // we can stop visiting now
                }
            }

            (Some(rl::Region::Static), _)
            | (Some(rl::Region::EarlyBound(_, _, _)), _)
            | (Some(rl::Region::LateBound(_, _, _)), _)
            | (Some(rl::Region::LateBoundAnon(_, _)), _)
            | (Some(rl::Region::Free(_, _)), _)
            | (None, _) => {
                debug!("no arg found");
            }
        }
    }

    fn visit_ty(&mut self, arg: &'gcx hir::Ty) {
        // ignore nested types
        //
        // If you have a type like `Foo<'a, &Ty>` we
        // are only interested in the immediate lifetimes ('a).
        //
        // Making `visit_ty` empty will ignore the `&Ty` embedded
        // inside, it will get reached by the outer visitor.
        debug!("`Ty` corresponding to a struct is {:?}", arg);
    }
}
