use super::combine::CombineFields;
use super::InferCtxt;
use super::lattice::{self, LatticeDir};
use super::Subtype;

use crate::traits::ObligationCause;
use crate::ty::{self, Ty, TyCtxt};
use crate::ty::relate::{Relate, RelateResult, TypeRelation};

/// "Greatest lower bound" (common subtype)
pub struct Glb<'combine, 'infcx: 'combine, 'gcx: 'infcx+'tcx, 'tcx: 'infcx> {
    fields: &'combine mut CombineFields<'infcx, 'gcx, 'tcx>,
    a_is_expected: bool,
}

impl<'combine, 'infcx, 'gcx, 'tcx> Glb<'combine, 'infcx, 'gcx, 'tcx> {
    pub fn new(fields: &'combine mut CombineFields<'infcx, 'gcx, 'tcx>, a_is_expected: bool)
        -> Glb<'combine, 'infcx, 'gcx, 'tcx>
    {
        Glb { fields: fields, a_is_expected: a_is_expected }
    }
}

impl TypeRelation<'gcx, 'tcx> for Glb<'combine, 'infcx, 'gcx, 'tcx> {
    fn tag(&self) -> &'static str { "Glb" }

    fn tcx(&self) -> TyCtxt<'gcx, 'tcx> { self.fields.tcx() }

    fn a_is_expected(&self) -> bool { self.a_is_expected }

    fn relate_with_variance<T: Relate<'tcx>>(&mut self,
                                             variance: ty::Variance,
                                             a: &T,
                                             b: &T)
                                             -> RelateResult<'tcx, T>
    {
        match variance {
            ty::Invariant => self.fields.equate(self.a_is_expected).relate(a, b),
            ty::Covariant => self.relate(a, b),
            // FIXME(#41044) -- not correct, need test
            ty::Bivariant => Ok(a.clone()),
            ty::Contravariant => self.fields.lub(self.a_is_expected).relate(a, b),
        }
    }

    fn tys(&mut self, a: Ty<'tcx>, b: Ty<'tcx>) -> RelateResult<'tcx, Ty<'tcx>> {
        lattice::super_lattice_tys(self, a, b)
    }

    fn regions(&mut self, a: ty::Region<'tcx>, b: ty::Region<'tcx>)
               -> RelateResult<'tcx, ty::Region<'tcx>> {
        debug!("{}.regions({:?}, {:?})",
               self.tag(),
               a,
               b);

        let origin = Subtype(self.fields.trace.clone());
        Ok(self.fields.infcx.borrow_region_constraints().glb_regions(self.tcx(), origin, a, b))
    }

    fn consts(
        &mut self,
        a: &'tcx ty::Const<'tcx>,
        b: &'tcx ty::Const<'tcx>,
    ) -> RelateResult<'tcx, &'tcx ty::Const<'tcx>> {
        debug!("{}.consts({:?}, {:?})", self.tag(), a, b);
        if a == b {
            return Ok(a);
        }

        self.fields.infcx.super_combine_consts(self, a, b)
    }

    fn binders<T>(&mut self, a: &ty::Binder<T>, b: &ty::Binder<T>)
                  -> RelateResult<'tcx, ty::Binder<T>>
        where T: Relate<'tcx>
    {
        debug!("binders(a={:?}, b={:?})", a, b);

        // When higher-ranked types are involved, computing the LUB is
        // very challenging, switch to invariance. This is obviously
        // overly conservative but works ok in practice.
        self.relate_with_variance(ty::Variance::Invariant, a, b)?;
        Ok(a.clone())
    }
}

impl<'combine, 'infcx, 'gcx, 'tcx> LatticeDir<'infcx, 'gcx, 'tcx>
    for Glb<'combine, 'infcx, 'gcx, 'tcx>
{
    fn infcx(&self) -> &'infcx InferCtxt<'infcx, 'gcx, 'tcx> {
        self.fields.infcx
    }

    fn cause(&self) -> &ObligationCause<'tcx> {
        &self.fields.trace.cause
    }

    fn relate_bound(&mut self, v: Ty<'tcx>, a: Ty<'tcx>, b: Ty<'tcx>) -> RelateResult<'tcx, ()> {
        let mut sub = self.fields.sub(self.a_is_expected);
        sub.relate(&v, &a)?;
        sub.relate(&v, &b)?;
        Ok(())
    }
}
