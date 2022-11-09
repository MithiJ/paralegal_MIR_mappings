use serde::Deserialize;

use crate::{
    ana::{
        extract_places, read_places_with_provenance, CallDeps, GlobalLocation, GlobalLocationS,
        IsGlobalLocation,
    },
    mir,
    rust::TyCtxt,
    serde::{Serialize, Serializer},
    Either, HashMap, HashSet, Symbol,
};

fn bbref_to_usize(r: &mir::BasicBlock) -> usize {
    r.as_usize()
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(remote = "mir::BasicBlock")]
struct BasicBlockProxy {
    #[serde(getter = "bbref_to_usize")]
    private: usize,
}

impl Into<mir::BasicBlock> for BasicBlockProxy {
    fn into(self) -> mir::BasicBlock {
        mir::BasicBlock::from_usize(self.private)
    }
}

#[derive(serde::Serialize, Eq, PartialEq, Hash, serde::Deserialize)]
pub struct LocationProxy {
    #[serde(with = "BasicBlockProxy")]
    pub block: mir::BasicBlock,
    pub statement_index: usize,
}

pub mod ser_loc {
    use crate::mir;
    use serde::{Deserialize, Serialize};

    pub fn deserialize<'de, D>(deserializer: D) -> Result<mir::Location, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        super::LocationProxy::deserialize(deserializer).map(|s| s.into())
    }

    pub fn serialize<S>(s: &mir::Location, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        super::LocationProxy::from(*s).serialize(serializer)
    }
}

impl From<mir::Location> for LocationProxy {
    fn from(l: mir::Location) -> Self {
        Self {
            block: l.block,
            statement_index: l.statement_index,
        }
    }
}

impl Into<mir::Location> for LocationProxy {
    fn into(self) -> mir::Location {
        let Self {
            block,
            statement_index,
        } = self;
        mir::Location {
            block,
            statement_index,
        }
    }
}

#[derive(Debug)]
pub struct BodyProxy(pub Vec<(mir::Location, String, HashSet<Symbol>)>);

impl Serialize for BodyProxy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0
            .iter()
            .map(|(l, s, h)| {
                (
                    (*l).into(),
                    s,
                    h.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<(LocationProxy, _, _)>>()
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for BodyProxy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        <Vec<(LocationProxy, String, Vec<SymbolProxy>)> as Deserialize<'de>>::deserialize(
            deserializer,
        )
        .map(|v| {
            v.into_iter()
                .map(|(l, s, vs)| (l.into(), s, vs.into_iter().map(|s| s.into()).collect()))
                .collect()
        })
        .map(BodyProxy)
    }
}
fn iter_stmts<'a, 'tcx>(
    b: &'a mir::Body<'tcx>,
) -> impl Iterator<
    Item = (
        mir::Location,
        Either<&'a mir::Statement<'tcx>, &'a mir::Terminator<'tcx>>,
    ),
> {
    b.basic_blocks()
        .iter_enumerated()
        .flat_map(|(block, bbdat)| {
            bbdat
                .statements
                .iter()
                .enumerate()
                .map(move |(statement_index, stmt)| {
                    (
                        mir::Location {
                            block,
                            statement_index,
                        },
                        Either::Left(stmt),
                    )
                })
                .chain(std::iter::once((
                    mir::Location {
                        block,
                        statement_index: bbdat.statements.len(),
                    },
                    Either::Right(bbdat.terminator()),
                )))
        })
}

impl<'tcx> From<&mir::Body<'tcx>> for BodyProxy {
    fn from(body: &mir::Body<'tcx>) -> Self {
        Self(
            iter_stmts(body)
                .map(|(loc, stmt)| {
                    (
                        loc,
                        stmt.either(|s| format!("{:?}", s.kind), |t| format!("{:?}", t.kind)),
                        extract_places(loc, body, false)
                            .into_iter()
                            .map(|p| Symbol::intern(&format!("{p:?}")))
                            .collect(),
                    )
                })
                .collect::<Vec<_>>(),
        )
    }
}

impl BodyProxy {
    pub fn from_body_with_normalize<'tcx>(body: &mir::Body<'tcx>, tcx: TyCtxt<'tcx>) -> Self {
        Self(
            iter_stmts(body)
                .map(|(loc, stmt)| {
                    (
                        loc,
                        stmt.either(|s| format!("{:?}", s.kind), |t| format!("{:?}", t.kind)),
                        read_places_with_provenance(loc, &body.stmt_at(loc), tcx)
                            .map(|p| Symbol::intern(&format!("{p:?}")))
                            .collect(),
                    )
                })
                .collect::<Vec<_>>(),
        )
    }
}

pub struct SymbolProxy(Symbol);

pub mod ser_sym {
    use crate::Symbol;
    use serde::{Deserialize, Serialize};

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Symbol, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        super::SymbolProxy::deserialize(deserializer).map(|s| s.into())
    }

    pub fn serialize<S>(s: &Symbol, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        super::SymbolProxy::from(*s).serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for SymbolProxy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(|s| Self(Symbol::intern(&s)))
    }
}

impl Serialize for SymbolProxy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.as_str().serialize(serializer)
    }
}

impl From<Symbol> for SymbolProxy {
    fn from(sym: Symbol) -> Self {
        Self(sym)
    }
}

impl Into<Symbol> for SymbolProxy {
    fn into(self) -> Symbol {
        self.0
    }
}

use crate::rust::hir::{self, def_id};

#[derive(Serialize, Deserialize)]
struct LocationDomainProxy {
    domain: Vec<LocationProxy>,
    #[serde(with = "BasicBlockProxy")]
    arg_block: mir::BasicBlock,
    real_locations: usize,
}

fn item_local_id_as_u32(i: &hir::ItemLocalId) -> u32 {
    i.as_u32()
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "hir::ItemLocalId")]
struct ItemLocalIdProxy {
    #[serde(getter = "item_local_id_as_u32")]
    private: u32,
}

impl Into<hir::ItemLocalId> for ItemLocalIdProxy {
    fn into(self) -> hir::ItemLocalId {
        hir::ItemLocalId::from_u32(self.private)
    }
}

fn def_index_as_u32(i: &def_id::DefIndex) -> u32 {
    i.as_u32()
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "def_id::DefIndex")]
struct DefIndexProxy {
    #[serde(getter = "def_index_as_u32")]
    private: u32,
}

impl Into<def_id::DefIndex> for DefIndexProxy {
    fn into(self) -> def_id::DefIndex {
        def_id::DefIndex::from_u32(self.private)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "def_id::LocalDefId")]
struct LocalDefIdProxy {
    #[serde(with = "DefIndexProxy")]
    local_def_index: def_id::DefIndex,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "hir::HirId")]
struct HirIdProxy {
    #[serde(with = "LocalDefIdProxy")]
    owner: def_id::LocalDefId,
    #[serde(with = "ItemLocalIdProxy")]
    local_id: hir::ItemLocalId,
}

#[derive(Deserialize, Serialize)]
#[serde(remote = "hir::BodyId")]
pub struct BodyIdProxy {
    #[serde(with = "HirIdProxy")]
    hir_id: hir::HirId,
}

/// This exists because of serde's restrictions on how you derive serializers.
/// `BodyIdProxy` can be used to serialize a `BodyId` but if the `BodyId` is
/// used as e.g. a key in a map or in a vector it does not dispatch to the
/// remote impl on `BodyIdProxy`. Implementing the serializers for the map or
/// vector by hand is annoying so instead you can map over the datastructure,
/// wrap each `BodyId` in this proxy type and then dispatch to the `serialize`
/// impl for the reconstructed data structure.
#[derive(Serialize, Deserialize)]
pub struct BodyIdProxy2(#[serde(with = "BodyIdProxy")] pub hir::BodyId);

#[derive(Deserialize, Serialize, PartialEq, Eq, Hash)]
pub struct RawGlobalLocation(Box<GlobalLocationS<RawGlobalLocation>>);

impl<'g> From<&'_ GlobalLocation<'g>> for RawGlobalLocation {
    fn from(other: &GlobalLocation<'g>) -> Self {
        RawGlobalLocation(Box::new(GlobalLocationS {
            function: other.function(),
            next: other.next().map(|o| o.into()),
            location: other.location(),
        }))
    }
}

impl crate::ana::IsGlobalLocation for RawGlobalLocation {
    fn as_global_location_s(&self) -> &GlobalLocationS<RawGlobalLocation> {
        &self.0
    }
}

impl<'g> Serialize for crate::ana::GlobalLocation<'g> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        RawGlobalLocation::from(self).serialize(serializer)
    }
}
pub struct SerializableCallOnlyFlow(pub HashMap<RawGlobalLocation, CallDeps<RawGlobalLocation>>);

impl Serialize for SerializableCallOnlyFlow {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer {
        serialize_map_via_vec(&self.0, serializer)
    }
}

impl <'de> Deserialize<'de> for SerializableCallOnlyFlow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de> {
        Ok(Self(deserialize_map_via_vec(deserializer)?))
    }
}

fn serialize_map_via_vec<S: Serializer, K: Serialize, V:Serialize>(map: &HashMap<K, V>, serializer: S) -> Result<S::Ok, S::Error> {
    map.iter().collect::<Vec<_>>().serialize(serializer)
}

fn deserialize_map_via_vec<'de, D: serde::Deserializer<'de>, K: Deserialize<'de> + std::cmp::Eq + std::hash::Hash, V: Deserialize<'de>>(deserializer: D) -> Result<HashMap<K, V>, D::Error> {
    Ok(Vec::deserialize(deserializer)?.into_iter().collect())
}

impl SerializableCallOnlyFlow {
    pub fn all_locations_iter(&self) -> impl Iterator<Item = &RawGlobalLocation> {
        self.0.iter().flat_map(|(from, deps)| {
            std::iter::once(from).chain(
                std::iter::once(&deps.ctrl_deps)
                    .chain(deps.input_deps.iter())
                    .flat_map(|d| d.iter()),
            )
        })
    }
}

impl From<&crate::ana::CallOnlyFlow<'_>> for SerializableCallOnlyFlow {
    fn from(other: &crate::ana::CallOnlyFlow<'_>) -> Self {
        SerializableCallOnlyFlow(
            other
                .iter()
                .map(|(g, v)| {
                    (
                        g.into(),
                        CallDeps {
                            ctrl_deps: v.ctrl_deps.iter().map(|l| l.into()).collect(),
                            input_deps: v
                                .input_deps
                                .iter()
                                .map(|hs| hs.iter().map(|d| d.into()).collect())
                                .collect(),
                        },
                    )
                })
                .collect(),
        )
    }
}

pub struct Bodies(pub HashMap<hir::BodyId, BodyProxy>);

impl Serialize for Bodies {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0
            .iter()
            .map(|(bid, b)| (BodyIdProxy2(*bid), b))
            .collect::<Vec<_>>()
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Bodies {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Vec::deserialize(deserializer).map(|v| {
            Bodies(
                v.into_iter()
                    .map(|(BodyIdProxy2(bid), v)| (bid, v))
                    .collect(),
            )
        })
    }
}