use crate::borrow_check::ArtificialField;
use crate::borrow_check::Overlap;
use crate::borrow_check::{Deep, Shallow, AccessDepth};
use rustc::hir;
use rustc::mir::{
    BorrowKind, Body, Place, PlaceBase, Projection, ProjectionElem, ProjectionsIter,
    StaticKind
};
use rustc::ty::{self, TyCtxt};
use std::cmp::max;

/// When checking if a place conflicts with another place, this enum is used to influence decisions
/// where a place might be equal or disjoint with another place, such as if `a[i] == a[j]`.
/// `PlaceConflictBias::Overlap` would bias toward assuming that `i` might equal `j` and that these
/// places overlap. `PlaceConflictBias::NoOverlap` assumes that for the purposes of the predicate
/// being run in the calling context, the conservative choice is to assume the compared indices
/// are disjoint (and therefore, do not overlap).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
crate enum PlaceConflictBias {
    Overlap,
    NoOverlap,
}

/// Helper function for checking if places conflict with a mutable borrow and deep access depth.
/// This is used to check for places conflicting outside of the borrow checking code (such as in
/// dataflow).
crate fn places_conflict<'gcx, 'tcx>(
    tcx: TyCtxt<'gcx, 'tcx>,
    body: &Body<'tcx>,
    borrow_place: &Place<'tcx>,
    access_place: &Place<'tcx>,
    bias: PlaceConflictBias,
) -> bool {
    borrow_conflicts_with_place(
        tcx,
        body,
        borrow_place,
        BorrowKind::Mut { allow_two_phase_borrow: true },
        access_place,
        AccessDepth::Deep,
        bias,
    )
}

/// Checks whether the `borrow_place` conflicts with the `access_place` given a borrow kind and
/// access depth. The `bias` parameter is used to determine how the unknowable (comparing runtime
/// array indices, for example) should be interpreted - this depends on what the caller wants in
/// order to make the conservative choice and preserve soundness.
pub(super) fn borrow_conflicts_with_place<'gcx, 'tcx>(
    tcx: TyCtxt<'gcx, 'tcx>,
    body: &Body<'tcx>,
    borrow_place: &Place<'tcx>,
    borrow_kind: BorrowKind,
    access_place: &Place<'tcx>,
    access: AccessDepth,
    bias: PlaceConflictBias,
) -> bool {
    debug!(
        "borrow_conflicts_with_place({:?}, {:?}, {:?}, {:?})",
        borrow_place, access_place, access, bias,
    );

    // This Local/Local case is handled by the more general code below, but
    // it's so common that it's a speed win to check for it first.
    if let Place::Base(PlaceBase::Local(l1)) = borrow_place {
        if let Place::Base(PlaceBase::Local(l2)) = access_place {
            return l1 == l2;
        }
    }

    borrow_place.iterate(|borrow_base, borrow_projections| {
        access_place.iterate(|access_base, access_projections| {
            place_components_conflict(
                tcx,
                body,
                (borrow_base, borrow_projections),
                borrow_kind,
                (access_base, access_projections),
                access,
                bias,
            )
        })
    })
}

fn place_components_conflict<'gcx, 'tcx>(
    tcx: TyCtxt<'gcx, 'tcx>,
    body: &Body<'tcx>,
    borrow_projections: (&PlaceBase<'tcx>, ProjectionsIter<'_, 'tcx>),
    borrow_kind: BorrowKind,
    access_projections: (&PlaceBase<'tcx>, ProjectionsIter<'_, 'tcx>),
    access: AccessDepth,
    bias: PlaceConflictBias,
) -> bool {
    // The borrowck rules for proving disjointness are applied from the "root" of the
    // borrow forwards, iterating over "similar" projections in lockstep until
    // we can prove overlap one way or another. Essentially, we treat `Overlap` as
    // a monoid and report a conflict if the product ends up not being `Disjoint`.
    //
    // At each step, if we didn't run out of borrow or place, we know that our elements
    // have the same type, and that they only overlap if they are the identical.
    //
    // For example, if we are comparing these:
    // BORROW:  (*x1[2].y).z.a
    // ACCESS:  (*x1[i].y).w.b
    //
    // Then our steps are:
    //       x1         |   x1          -- places are the same
    //       x1[2]      |   x1[i]       -- equal or disjoint (disjoint if indexes differ)
    //       x1[2].y    |   x1[i].y     -- equal or disjoint
    //      *x1[2].y    |  *x1[i].y     -- equal or disjoint
    //     (*x1[2].y).z | (*x1[i].y).w  -- we are disjoint and don't need to check more!
    //
    // Because `zip` does potentially bad things to the iterator inside, this loop
    // also handles the case where the access might be a *prefix* of the borrow, e.g.
    //
    // BORROW:  (*x1[2].y).z.a
    // ACCESS:  x1[i].y
    //
    // Then our steps are:
    //       x1         |   x1          -- places are the same
    //       x1[2]      |   x1[i]       -- equal or disjoint (disjoint if indexes differ)
    //       x1[2].y    |   x1[i].y     -- equal or disjoint
    //
    // -- here we run out of access - the borrow can access a part of it. If this
    // is a full deep access, then we *know* the borrow conflicts with it. However,
    // if the access is shallow, then we can proceed:
    //
    //       x1[2].y    | (*x1[i].y)    -- a deref! the access can't get past this, so we
    //                                     are disjoint
    //
    // Our invariant is, that at each step of the iteration:
    //  - If we didn't run out of access to match, our borrow and access are comparable
    //    and either equal or disjoint.
    //  - If we did run out of access, the borrow can access a part of it.

    let borrow_base = borrow_projections.0;
    let access_base = access_projections.0;

    match place_base_conflict(tcx, borrow_base, access_base) {
        Overlap::Arbitrary => {
            bug!("Two base can't return Arbitrary");
        }
        Overlap::EqualOrDisjoint => {
            // This is the recursive case - proceed to the next element.
        }
        Overlap::Disjoint => {
            // We have proven the borrow disjoint - further
            // projections will remain disjoint.
            debug!("borrow_conflicts_with_place: disjoint");
            return false;
        }
    }

    let mut borrow_projections = borrow_projections.1;
    let mut access_projections = access_projections.1;

    loop {
        // loop invariant: borrow_c is always either equal to access_c or disjoint from it.
        if let Some(borrow_c) = borrow_projections.next() {
            debug!("borrow_conflicts_with_place: borrow_c = {:?}", borrow_c);

            if let Some(access_c) = access_projections.next() {
                debug!("borrow_conflicts_with_place: access_c = {:?}", access_c);

                // Borrow and access path both have more components.
                //
                // Examples:
                //
                // - borrow of `a.(...)`, access to `a.(...)`
                // - borrow of `a.(...)`, access to `b.(...)`
                //
                // Here we only see the components we have checked so
                // far (in our examples, just the first component). We
                // check whether the components being borrowed vs
                // accessed are disjoint (as in the second example,
                // but not the first).
                match place_projection_conflict(tcx, body, borrow_c, access_c, bias) {
                    Overlap::Arbitrary => {
                        // We have encountered different fields of potentially
                        // the same union - the borrow now partially overlaps.
                        //
                        // There is no *easy* way of comparing the fields
                        // further on, because they might have different types
                        // (e.g., borrows of `u.a.0` and `u.b.y` where `.0` and
                        // `.y` come from different structs).
                        //
                        // We could try to do some things here - e.g., count
                        // dereferences - but that's probably not a good
                        // idea, at least for now, so just give up and
                        // report a conflict. This is unsafe code anyway so
                        // the user could always use raw pointers.
                        debug!("borrow_conflicts_with_place: arbitrary -> conflict");
                        return true;
                    }
                    Overlap::EqualOrDisjoint => {
                        // This is the recursive case - proceed to the next element.
                    }
                    Overlap::Disjoint => {
                        // We have proven the borrow disjoint - further
                        // projections will remain disjoint.
                        debug!("borrow_conflicts_with_place: disjoint");
                        return false;
                    }
                }
            } else {
                // Borrow path is longer than the access path. Examples:
                //
                // - borrow of `a.b.c`, access to `a.b`
                //
                // Here, we know that the borrow can access a part of
                // our place. This is a conflict if that is a part our
                // access cares about.

                let base = &borrow_c.base;
                let elem = &borrow_c.elem;
                let base_ty = base.ty(body, tcx).ty;

                match (elem, &base_ty.sty, access) {
                    (_, _, Shallow(Some(ArtificialField::ArrayLength)))
                    | (_, _, Shallow(Some(ArtificialField::ShallowBorrow))) => {
                        // The array length is like  additional fields on the
                        // type; it does not overlap any existing data there.
                        // Furthermore, if cannot actually be a prefix of any
                        // borrowed place (at least in MIR as it is currently.)
                        //
                        // e.g., a (mutable) borrow of `a[5]` while we read the
                        // array length of `a`.
                        debug!("borrow_conflicts_with_place: implicit field");
                        return false;
                    }

                    (ProjectionElem::Deref, _, Shallow(None)) => {
                        // e.g., a borrow of `*x.y` while we shallowly access `x.y` or some
                        // prefix thereof - the shallow access can't touch anything behind
                        // the pointer.
                        debug!("borrow_conflicts_with_place: shallow access behind ptr");
                        return false;
                    }
                    (ProjectionElem::Deref, ty::Ref(_, _, hir::MutImmutable), _) => {
                        // Shouldn't be tracked
                        bug!("Tracking borrow behind shared reference.");
                    }
                    (ProjectionElem::Deref, ty::Ref(_, _, hir::MutMutable), AccessDepth::Drop) => {
                        // Values behind a mutable reference are not access either by dropping a
                        // value, or by StorageDead
                        debug!("borrow_conflicts_with_place: drop access behind ptr");
                        return false;
                    }

                    (ProjectionElem::Field { .. }, ty::Adt(def, _), AccessDepth::Drop) => {
                        // Drop can read/write arbitrary projections, so places
                        // conflict regardless of further projections.
                        if def.has_dtor(tcx) {
                            return true;
                        }
                    }

                    (ProjectionElem::Deref, _, Deep)
                    | (ProjectionElem::Deref, _, AccessDepth::Drop)
                    | (ProjectionElem::Field { .. }, _, _)
                    | (ProjectionElem::Index { .. }, _, _)
                    | (ProjectionElem::ConstantIndex { .. }, _, _)
                    | (ProjectionElem::Subslice { .. }, _, _)
                    | (ProjectionElem::Downcast { .. }, _, _) => {
                        // Recursive case. This can still be disjoint on a
                        // further iteration if this a shallow access and
                        // there's a deref later on, e.g., a borrow
                        // of `*x.y` while accessing `x`.
                    }
                }
            }
        } else {
            // Borrow path ran out but access path may not
            // have. Examples:
            //
            // - borrow of `a.b`, access to `a.b.c`
            // - borrow of `a.b`, access to `a.b`
            //
            // In the first example, where we didn't run out of
            // access, the borrow can access all of our place, so we
            // have a conflict.
            //
            // If the second example, where we did, then we still know
            // that the borrow can access a *part* of our place that
            // our access cares about, so we still have a conflict.
            if borrow_kind == BorrowKind::Shallow && access_projections.next().is_some() {
                debug!("borrow_conflicts_with_place: shallow borrow");
                return false;
            } else {
                debug!("borrow_conflicts_with_place: full borrow, CONFLICT");
                return true;
            }
        }
    }
}

// Given that the bases of `elem1` and `elem2` are always either equal
// or disjoint (and have the same type!), return the overlap situation
// between `elem1` and `elem2`.
fn place_base_conflict<'gcx: 'tcx, 'tcx>(
    tcx: TyCtxt<'gcx, 'tcx>,
    elem1: &PlaceBase<'tcx>,
    elem2: &PlaceBase<'tcx>,
) -> Overlap {
    match (elem1, elem2) {
        (PlaceBase::Local(l1), PlaceBase::Local(l2)) => {
            if l1 == l2 {
                // the same local - base case, equal
                debug!("place_element_conflict: DISJOINT-OR-EQ-LOCAL");
                Overlap::EqualOrDisjoint
            } else {
                // different locals - base case, disjoint
                debug!("place_element_conflict: DISJOINT-LOCAL");
                Overlap::Disjoint
            }
        }
        (PlaceBase::Static(s1), PlaceBase::Static(s2)) => {
            match (&s1.kind, &s2.kind) {
                (StaticKind::Static(def_id_1), StaticKind::Static(def_id_2)) => {
                    if def_id_1 != def_id_2 {
                        debug!("place_element_conflict: DISJOINT-STATIC");
                        Overlap::Disjoint
                    } else if tcx.is_mutable_static(*def_id_1) {
                        // We ignore mutable statics - they can only be unsafe code.
                        debug!("place_element_conflict: IGNORE-STATIC-MUT");
                        Overlap::Disjoint
                    } else {
                        debug!("place_element_conflict: DISJOINT-OR-EQ-STATIC");
                        Overlap::EqualOrDisjoint
                    }
                },
                (StaticKind::Promoted(promoted_1), StaticKind::Promoted(promoted_2)) => {
                    if promoted_1 == promoted_2 {
                        if let ty::Array(_, len) = s1.ty.sty {
                            if let Some(0) = len.assert_usize(tcx) {
                                // Ignore conflicts with promoted [T; 0].
                                debug!("place_element_conflict: IGNORE-LEN-0-PROMOTED");
                                return Overlap::Disjoint;
                            }
                        }
                        // the same promoted - base case, equal
                        debug!("place_element_conflict: DISJOINT-OR-EQ-PROMOTED");
                        Overlap::EqualOrDisjoint
                    } else {
                        // different promoteds - base case, disjoint
                        debug!("place_element_conflict: DISJOINT-PROMOTED");
                        Overlap::Disjoint
                    }
                },
                (_, _) => {
                    debug!("place_element_conflict: DISJOINT-STATIC-PROMOTED");
                    Overlap::Disjoint
                }
            }
        }
        (PlaceBase::Local(_), PlaceBase::Static(_)) |
        (PlaceBase::Static(_), PlaceBase::Local(_)) => {
            debug!("place_element_conflict: DISJOINT-STATIC-LOCAL-PROMOTED");
            Overlap::Disjoint
        }
    }
}

// Given that the bases of `elem1` and `elem2` are always either equal
// or disjoint (and have the same type!), return the overlap situation
// between `elem1` and `elem2`.
fn place_projection_conflict<'gcx: 'tcx, 'tcx>(
    tcx: TyCtxt<'gcx, 'tcx>,
    body: &Body<'tcx>,
    pi1: &Projection<'tcx>,
    pi2: &Projection<'tcx>,
    bias: PlaceConflictBias,
) -> Overlap {
    match (&pi1.elem, &pi2.elem) {
        (ProjectionElem::Deref, ProjectionElem::Deref) => {
            // derefs (e.g., `*x` vs. `*x`) - recur.
            debug!("place_element_conflict: DISJOINT-OR-EQ-DEREF");
            Overlap::EqualOrDisjoint
        }
        (ProjectionElem::Field(f1, _), ProjectionElem::Field(f2, _)) => {
            if f1 == f2 {
                // same field (e.g., `a.y` vs. `a.y`) - recur.
                debug!("place_element_conflict: DISJOINT-OR-EQ-FIELD");
                Overlap::EqualOrDisjoint
            } else {
                let ty = pi1.base.ty(body, tcx).ty;
                match ty.sty {
                    ty::Adt(def, _) if def.is_union() => {
                        // Different fields of a union, we are basically stuck.
                        debug!("place_element_conflict: STUCK-UNION");
                        Overlap::Arbitrary
                    }
                    _ => {
                        // Different fields of a struct (`a.x` vs. `a.y`). Disjoint!
                        debug!("place_element_conflict: DISJOINT-FIELD");
                        Overlap::Disjoint
                    }
                }
            }
        }
        (ProjectionElem::Downcast(_, v1), ProjectionElem::Downcast(_, v2)) => {
            // different variants are treated as having disjoint fields,
            // even if they occupy the same "space", because it's
            // impossible for 2 variants of the same enum to exist
            // (and therefore, to be borrowed) at the same time.
            //
            // Note that this is different from unions - we *do* allow
            // this code to compile:
            //
            // ```
            // fn foo(x: &mut Result<i32, i32>) {
            //     let mut v = None;
            //     if let Ok(ref mut a) = *x {
            //         v = Some(a);
            //     }
            //     // here, you would *think* that the
            //     // *entirety* of `x` would be borrowed,
            //     // but in fact only the `Ok` variant is,
            //     // so the `Err` variant is *entirely free*:
            //     if let Err(ref mut a) = *x {
            //         v = Some(a);
            //     }
            //     drop(v);
            // }
            // ```
            if v1 == v2 {
                debug!("place_element_conflict: DISJOINT-OR-EQ-FIELD");
                Overlap::EqualOrDisjoint
            } else {
                debug!("place_element_conflict: DISJOINT-FIELD");
                Overlap::Disjoint
            }
        }
        (ProjectionElem::Index(..), ProjectionElem::Index(..))
        | (ProjectionElem::Index(..), ProjectionElem::ConstantIndex { .. })
        | (ProjectionElem::Index(..), ProjectionElem::Subslice { .. })
        | (ProjectionElem::ConstantIndex { .. }, ProjectionElem::Index(..))
        | (ProjectionElem::Subslice { .. }, ProjectionElem::Index(..)) => {
            // Array indexes (`a[0]` vs. `a[i]`). These can either be disjoint
            // (if the indexes differ) or equal (if they are the same).
            match bias {
                PlaceConflictBias::Overlap => {
                    // If we are biased towards overlapping, then this is the recursive
                    // case that gives "equal *or* disjoint" its meaning.
                    debug!("place_element_conflict: DISJOINT-OR-EQ-ARRAY-INDEX");
                    Overlap::EqualOrDisjoint
                }
                PlaceConflictBias::NoOverlap => {
                    // If we are biased towards no overlapping, then this is disjoint.
                    debug!("place_element_conflict: DISJOINT-ARRAY-INDEX");
                    Overlap::Disjoint
                }
            }
        }
        (ProjectionElem::ConstantIndex { offset: o1, min_length: _, from_end: false },
            ProjectionElem::ConstantIndex { offset: o2, min_length: _, from_end: false })
        | (ProjectionElem::ConstantIndex { offset: o1, min_length: _, from_end: true },
            ProjectionElem::ConstantIndex {
                offset: o2, min_length: _, from_end: true }) => {
            if o1 == o2 {
                debug!("place_element_conflict: DISJOINT-OR-EQ-ARRAY-CONSTANT-INDEX");
                Overlap::EqualOrDisjoint
            } else {
                debug!("place_element_conflict: DISJOINT-ARRAY-CONSTANT-INDEX");
                Overlap::Disjoint
            }
        }
        (ProjectionElem::ConstantIndex {
            offset: offset_from_begin, min_length: min_length1, from_end: false },
            ProjectionElem::ConstantIndex {
                offset: offset_from_end, min_length: min_length2, from_end: true })
        | (ProjectionElem::ConstantIndex {
            offset: offset_from_end, min_length: min_length1, from_end: true },
           ProjectionElem::ConstantIndex {
               offset: offset_from_begin, min_length: min_length2, from_end: false }) => {
            // both patterns matched so it must be at least the greater of the two
            let min_length = max(min_length1, min_length2);
            // `offset_from_end` can be in range `[1..min_length]`, 1 indicates the last
            // element (like -1 in Python) and `min_length` the first.
            // Therefore, `min_length - offset_from_end` gives the minimal possible
            // offset from the beginning
            if *offset_from_begin >= min_length - offset_from_end {
                debug!("place_element_conflict: DISJOINT-OR-EQ-ARRAY-CONSTANT-INDEX-FE");
                Overlap::EqualOrDisjoint
            } else {
                debug!("place_element_conflict: DISJOINT-ARRAY-CONSTANT-INDEX-FE");
                Overlap::Disjoint
            }
        }
        (ProjectionElem::ConstantIndex { offset, min_length: _, from_end: false },
         ProjectionElem::Subslice {from, .. })
        | (ProjectionElem::Subslice {from, .. },
            ProjectionElem::ConstantIndex { offset, min_length: _, from_end: false }) => {
            if offset >= from {
                debug!(
                    "place_element_conflict: DISJOINT-OR-EQ-ARRAY-CONSTANT-INDEX-SUBSLICE");
                Overlap::EqualOrDisjoint
            } else {
                debug!("place_element_conflict: DISJOINT-ARRAY-CONSTANT-INDEX-SUBSLICE");
                Overlap::Disjoint
            }
        }
        (ProjectionElem::ConstantIndex { offset, min_length: _, from_end: true },
         ProjectionElem::Subslice {from: _, to })
        | (ProjectionElem::Subslice {from: _, to },
            ProjectionElem::ConstantIndex { offset, min_length: _, from_end: true }) => {
            if offset > to {
                debug!("place_element_conflict: \
                       DISJOINT-OR-EQ-ARRAY-CONSTANT-INDEX-SUBSLICE-FE");
                Overlap::EqualOrDisjoint
            } else {
                debug!("place_element_conflict: DISJOINT-ARRAY-CONSTANT-INDEX-SUBSLICE-FE");
                Overlap::Disjoint
            }
        }
        (ProjectionElem::Subslice { .. }, ProjectionElem::Subslice { .. }) => {
            debug!("place_element_conflict: DISJOINT-OR-EQ-ARRAY-SUBSLICES");
             Overlap::EqualOrDisjoint
        }
        (ProjectionElem::Deref, _)
        | (ProjectionElem::Field(..), _)
        | (ProjectionElem::Index(..), _)
        | (ProjectionElem::ConstantIndex { .. }, _)
        | (ProjectionElem::Subslice { .. }, _)
        | (ProjectionElem::Downcast(..), _) => bug!(
            "mismatched projections in place_element_conflict: {:?} and {:?}",
            pi1,
            pi2
        ),
    }
}
