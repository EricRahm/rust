use rustc::ty::{Ty, TyCtxt};
use rustc::mir::*;
use rustc::util::nodemap::FxHashMap;
use rustc_data_structures::indexed_vec::{Idx, IndexVec};
use smallvec::SmallVec;
use syntax_pos::{Span};

use std::fmt;
use std::ops::{Index, IndexMut};

use self::abs_domain::{AbstractElem, Lift};

mod abs_domain;

newtype_index! {
    pub struct MovePathIndex {
        DEBUG_FORMAT = "mp{}"
    }
}

newtype_index! {
    pub struct MoveOutIndex {
        DEBUG_FORMAT = "mo{}"
    }
}

newtype_index! {
    pub struct InitIndex {
        DEBUG_FORMAT = "in{}"
    }
}

impl MoveOutIndex {
    pub fn move_path_index(&self, move_data: &MoveData<'_>) -> MovePathIndex {
        move_data.moves[*self].path
    }
}

/// `MovePath` is a canonicalized representation of a path that is
/// moved or assigned to.
///
/// It follows a tree structure.
///
/// Given `struct X { m: M, n: N }` and `x: X`, moves like `drop x.m;`
/// move *out* of the place `x.m`.
///
/// The MovePaths representing `x.m` and `x.n` are siblings (that is,
/// one of them will link to the other via the `next_sibling` field,
/// and the other will have no entry in its `next_sibling` field), and
/// they both have the MovePath representing `x` as their parent.
#[derive(Clone)]
pub struct MovePath<'tcx> {
    pub next_sibling: Option<MovePathIndex>,
    pub first_child: Option<MovePathIndex>,
    pub parent: Option<MovePathIndex>,
    pub place: Place<'tcx>,
}

impl<'tcx> MovePath<'tcx> {
    pub fn parents(
        &self,
        move_paths: &IndexVec<MovePathIndex, MovePath<'_>>,
    ) -> Vec<MovePathIndex> {
        let mut parents = Vec::new();

        let mut curr_parent = self.parent;
        while let Some(parent_mpi) = curr_parent {
            parents.push(parent_mpi);
            curr_parent = move_paths[parent_mpi].parent;
        }

        parents
    }
}

impl<'tcx> fmt::Debug for MovePath<'tcx> {
    fn fmt(&self, w: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(w, "MovePath {{")?;
        if let Some(parent) = self.parent {
            write!(w, " parent: {:?},", parent)?;
        }
        if let Some(first_child) = self.first_child {
            write!(w, " first_child: {:?},", first_child)?;
        }
        if let Some(next_sibling) = self.next_sibling {
            write!(w, " next_sibling: {:?}", next_sibling)?;
        }
        write!(w, " place: {:?} }}", self.place)
    }
}

impl<'tcx> fmt::Display for MovePath<'tcx> {
    fn fmt(&self, w: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(w, "{:?}", self.place)
    }
}

#[derive(Debug)]
pub struct MoveData<'tcx> {
    pub move_paths: IndexVec<MovePathIndex, MovePath<'tcx>>,
    pub moves: IndexVec<MoveOutIndex, MoveOut>,
    /// Each Location `l` is mapped to the MoveOut's that are effects
    /// of executing the code at `l`. (There can be multiple MoveOut's
    /// for a given `l` because each MoveOut is associated with one
    /// particular path being moved.)
    pub loc_map: LocationMap<SmallVec<[MoveOutIndex; 4]>>,
    pub path_map: IndexVec<MovePathIndex, SmallVec<[MoveOutIndex; 4]>>,
    pub rev_lookup: MovePathLookup,
    pub inits: IndexVec<InitIndex, Init>,
    /// Each Location `l` is mapped to the Inits that are effects
    /// of executing the code at `l`.
    pub init_loc_map: LocationMap<SmallVec<[InitIndex; 4]>>,
    pub init_path_map: IndexVec<MovePathIndex, SmallVec<[InitIndex; 4]>>,
}

pub trait HasMoveData<'tcx> {
    fn move_data(&self) -> &MoveData<'tcx>;
}

#[derive(Debug)]
pub struct LocationMap<T> {
    /// Location-indexed (BasicBlock for outer index, index within BB
    /// for inner index) map.
    pub(crate) map: IndexVec<BasicBlock, Vec<T>>,
}

impl<T> Index<Location> for LocationMap<T> {
    type Output = T;
    fn index(&self, index: Location) -> &Self::Output {
        &self.map[index.block][index.statement_index]
    }
}

impl<T> IndexMut<Location> for LocationMap<T> {
    fn index_mut(&mut self, index: Location) -> &mut Self::Output {
        &mut self.map[index.block][index.statement_index]
    }
}

impl<T> LocationMap<T> where T: Default + Clone {
    fn new(body: &Body<'_>) -> Self {
        LocationMap {
            map: body.basic_blocks().iter().map(|block| {
                vec![T::default(); block.statements.len()+1]
            }).collect()
        }
    }
}

/// `MoveOut` represents a point in a program that moves out of some
/// L-value; i.e., "creates" uninitialized memory.
///
/// With respect to dataflow analysis:
/// - Generated by moves and declaration of uninitialized variables.
/// - Killed by assignments to the memory.
#[derive(Copy, Clone)]
pub struct MoveOut {
    /// path being moved
    pub path: MovePathIndex,
    /// location of move
    pub source: Location,
}

impl fmt::Debug for MoveOut {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "{:?}@{:?}", self.path, self.source)
    }
}

/// `Init` represents a point in a program that initializes some L-value;
#[derive(Copy, Clone)]
pub struct Init {
    /// path being initialized
    pub path: MovePathIndex,
    /// location of initialization
    pub location: InitLocation,
    /// Extra information about this initialization
    pub kind: InitKind,
}


/// Initializations can be from an argument or from a statement. Arguments
/// do not have locations, in those cases the `Local` is kept..
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InitLocation {
    Argument(Local),
    Statement(Location),
}

/// Additional information about the initialization.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InitKind {
    /// Deep init, even on panic
    Deep,
    /// Only does a shallow init
    Shallow,
    /// This doesn't initialize the variable on panic (and a panic is possible).
    NonPanicPathOnly,
}

impl fmt::Debug for Init {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "{:?}@{:?} ({:?})", self.path, self.location, self.kind)
    }
}

impl Init {
    crate fn span<'gcx>(&self, body: &Body<'gcx>) -> Span {
        match self.location {
            InitLocation::Argument(local) => body.local_decls[local].source_info.span,
            InitLocation::Statement(location) => body.source_info(location).span,
        }
    }
}

/// Tables mapping from a place to its MovePathIndex.
#[derive(Debug)]
pub struct MovePathLookup {
    locals: IndexVec<Local, MovePathIndex>,

    /// projections are made from a base-place and a projection
    /// elem. The base-place will have a unique MovePathIndex; we use
    /// the latter as the index into the outer vector (narrowing
    /// subsequent search so that it is solely relative to that
    /// base-place). For the remaining lookup, we map the projection
    /// elem to the associated MovePathIndex.
    projections: FxHashMap<(MovePathIndex, AbstractElem), MovePathIndex>
}

mod builder;

#[derive(Copy, Clone, Debug)]
pub enum LookupResult {
    Exact(MovePathIndex),
    Parent(Option<MovePathIndex>)
}

impl MovePathLookup {
    // Unlike the builder `fn move_path_for` below, this lookup
    // alternative will *not* create a MovePath on the fly for an
    // unknown place, but will rather return the nearest available
    // parent.
    pub fn find(&self, place: &Place<'tcx>) -> LookupResult {
        place.iterate(|place_base, place_projection| {
            let mut result = match place_base {
                PlaceBase::Local(local) => self.locals[*local],
                PlaceBase::Static(..) => return LookupResult::Parent(None),
            };

            for proj in place_projection {
                if let Some(&subpath) = self.projections.get(&(result, proj.elem.lift())) {
                    result = subpath;
                } else {
                    return LookupResult::Parent(Some(result));
                }
            }

            LookupResult::Exact(result)
        })
    }

    pub fn find_local(&self, local: Local) -> MovePathIndex {
        self.locals[local]
    }
}

#[derive(Debug)]
pub struct IllegalMoveOrigin<'tcx> {
    pub(crate) location: Location,
    pub(crate) kind: IllegalMoveOriginKind<'tcx>,
}

#[derive(Debug)]
pub(crate) enum IllegalMoveOriginKind<'tcx> {
    /// Illegal move due to attempt to move from `static` variable.
    Static,

    /// Illegal move due to attempt to move from behind a reference.
    BorrowedContent {
        /// The place the reference refers to: if erroneous code was trying to
        /// move from `(*x).f` this will be `*x`.
        target_place: Place<'tcx>,
    },

    /// Illegal move due to attempt to move from field of an ADT that
    /// implements `Drop`. Rust maintains invariant that all `Drop`
    /// ADT's remain fully-initialized so that user-defined destructor
    /// can safely read from all of the ADT's fields.
    InteriorOfTypeWithDestructor { container_ty: Ty<'tcx> },

    /// Illegal move due to attempt to move out of a slice or array.
    InteriorOfSliceOrArray { ty: Ty<'tcx>, is_index: bool, },
}

#[derive(Debug)]
pub enum MoveError<'tcx> {
    IllegalMove { cannot_move_out_of: IllegalMoveOrigin<'tcx> },
    UnionMove { path: MovePathIndex },
}

impl<'tcx> MoveError<'tcx> {
    fn cannot_move_out_of(location: Location, kind: IllegalMoveOriginKind<'tcx>) -> Self {
        let origin = IllegalMoveOrigin { location, kind };
        MoveError::IllegalMove { cannot_move_out_of: origin }
    }
}

impl<'gcx, 'tcx> MoveData<'tcx> {
    pub fn gather_moves(
        body: &Body<'tcx>,
        tcx: TyCtxt<'gcx, 'tcx>,
    ) -> Result<Self, (Self, Vec<(Place<'tcx>, MoveError<'tcx>)>)> {
        builder::gather_moves(body, tcx)
    }

    /// For the move path `mpi`, returns the root local variable (if any) that starts the path.
    /// (e.g., for a path like `a.b.c` returns `Some(a)`)
    pub fn base_local(&self, mut mpi: MovePathIndex) -> Option<Local> {
        loop {
            let path = &self.move_paths[mpi];
            if let Place::Base(PlaceBase::Local(l)) = path.place { return Some(l); }
            if let Some(parent) = path.parent { mpi = parent; continue } else { return None }
        }
    }
}
