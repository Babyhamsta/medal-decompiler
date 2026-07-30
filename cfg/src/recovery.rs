use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    hash::{Hash, Hasher},
};

use ast::{LValue, LocalRw, RValue, RcLocal, Statement};
use petgraph::{
    algo::{dominators::simple_fast, kosaraju_scc},
    stable_graph::NodeIndex,
    visit::{EdgeRef, IntoEdgeReferences},
};
use thiserror::Error;

use crate::{
    block::BranchType,
    function::Function,
    provenance::{BindingIdentity, OriginSet},
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StatementLocation {
    pub block: NodeIndex,
    pub statement: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalFact {
    pub bindings: BTreeSet<BindingIdentity>,
    pub definitions: OriginSet,
    pub uses: OriginSet,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum BranchKind {
    Unconditional,
    Then,
    Else,
}

impl From<&BranchType> for BranchKind {
    fn from(value: &BranchType) -> Self {
        match value {
            BranchType::Unconditional => Self::Unconditional,
            BranchType::Then => Self::Then,
            BranchType::Else => Self::Else,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MergeRelationship {
    pub parameter: RcLocal,
    pub argument: RValue,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EdgeFact {
    pub source: NodeIndex,
    pub target: NodeIndex,
    pub branch: BranchKind,
    pub merges: Vec<MergeRelationship>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ResultProducer {
    Call,
    MethodCall,
    VarArg,
    GeneratorCall,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ResultShape {
    Exact(usize),
    Open,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ResultGroupFact {
    pub location: StatementLocation,
    pub ordinal: usize,
    pub producer: ResultProducer,
    pub demand: ResultShape,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EffectSummary {
    pub calls: bool,
    pub metamethod_capable: bool,
    pub local_reads: BTreeSet<BindingIdentity>,
    pub local_writes: BTreeSet<BindingIdentity>,
    pub upvalue_reads: BTreeSet<BindingIdentity>,
    pub upvalue_writes: BTreeSet<BindingIdentity>,
    pub table_root_reads: BTreeSet<BindingIdentity>,
    pub table_root_writes: BTreeSet<BindingIdentity>,
    pub table_root_escapes: BTreeSet<BindingIdentity>,
    pub allocation: bool,
    pub closure_capture: bool,
    pub result_groups: Vec<ResultGroupFact>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateRegion {
    pub header: NodeIndex,
    pub members: BTreeSet<NodeIndex>,
}

/// Selects which optional facts [`RecoveryFacts::derive_subset`] builds.
///
/// Facts must be built while the function is still in SSA form, so they cannot
/// be computed on first access: by the time a consumer reads them, SSA
/// destruction has already discarded the information they describe. Demand is
/// therefore declared up front, and an unrequested fact reads back as `None`
/// rather than as an empty collection that would be indistinguishable from a
/// genuinely empty result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FactSet(u8);

impl FactSet {
    pub const LOCALS: Self = Self(1 << 0);
    pub const STATEMENT_ORIGINS: Self = Self(1 << 1);
    pub const EDGES: Self = Self(1 << 2);
    pub const DOMINATORS: Self = Self(1 << 3);
    pub const POST_DOMINATORS: Self = Self(1 << 4);
    pub const EFFECTS: Self = Self(1 << 5);

    pub const NONE: Self = Self(0);
    pub const ALL: Self = Self(0b0011_1111);

    /// What source reconstruction actually reads.
    ///
    /// `candidate_regions` is always derived and is not part of this set.
    /// `restructure` pairs regions with edge facts to find the returns a
    /// region exits through, so edges belong here too.
    pub const RECONSTRUCTION: Self = Self::EDGES;

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecoveryFacts {
    function_id: usize,
    derived: FactSet,
    locals: BTreeMap<RcLocal, LocalFact>,
    statement_origins: BTreeMap<StatementLocation, OriginSet>,
    edges: Vec<EdgeFact>,
    dominators: BTreeMap<NodeIndex, BTreeSet<NodeIndex>>,
    post_dominators: BTreeMap<NodeIndex, BTreeSet<NodeIndex>>,
    candidate_regions: Vec<CandidateRegion>,
    effects: BTreeMap<StatementLocation, EffectSummary>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RecoveryError {
    #[error("function {function_id} entry block {entry:?} is not present in the CFG")]
    InvalidEntry {
        function_id: usize,
        entry: NodeIndex,
    },
}

impl RecoveryFacts {
    /// Derives the complete evidence surface.
    pub fn derive(function: &Function) -> Result<Self, RecoveryError> {
        Self::derive_subset(function, FactSet::ALL)
    }

    /// Derives `candidate_regions` plus the requested optional facts.
    ///
    /// `candidate_regions` is unconditional because it is what source
    /// reconstruction consumes and it costs a single SCC pass.
    pub fn derive_subset(function: &Function, requested: FactSet) -> Result<Self, RecoveryError> {
        if let Some(entry) = *function.entry()
            && !function.has_block(entry)
        {
            return Err(RecoveryError::InvalidEntry {
                function_id: function.id,
                entry,
            });
        }

        // `locals` accumulates across both the statement scan and the edge
        // scan, so either consumer forces the scan that feeds it.
        let wants_locals = requested.contains(FactSet::LOCALS);
        let scan_statements = wants_locals
            || requested.contains(FactSet::STATEMENT_ORIGINS)
            || requested.contains(FactSet::EFFECTS);
        let scan_edges = wants_locals || requested.contains(FactSet::EDGES);

        let (mut local_names, statement_origins, effects) = if scan_statements {
            crate::metrics::time(crate::metrics::Metric::FactsStatements, || {
                let mut local_names = function.parameters.iter().cloned().collect::<BTreeSet<_>>();
                let mut statement_origins = BTreeMap::new();
                let mut effects = BTreeMap::new();
                for (node, block) in function.blocks() {
                    for (statement, value) in block.iter().enumerate() {
                        let location = StatementLocation {
                            block: node,
                            statement,
                        };
                        local_names.extend(value.values().into_iter().cloned());
                        if let Some(origins) = function.statement_origins(node, statement) {
                            statement_origins.insert(location, origins.clone());
                        }
                        effects.insert(location, summarize_effects(function, location, value));
                    }
                }
                (local_names, statement_origins, effects)
            })
        } else {
            Default::default()
        };

        let edges = if scan_edges {
            crate::metrics::time(crate::metrics::Metric::FactsEdges, || {
                let mut edges = function
                    .graph()
                    .edge_references()
                    .map(|edge| {
                        let merges = edge
                            .weight()
                            .arguments
                            .iter()
                            .map(|(parameter, argument)| {
                                local_names.insert(parameter.clone());
                                local_names.extend(argument.values_read().into_iter().cloned());
                                MergeRelationship {
                                    parameter: parameter.clone(),
                                    argument: argument.clone(),
                                }
                            })
                            .collect();
                        EdgeFact {
                            source: edge.source(),
                            target: edge.target(),
                            branch: (&edge.weight().branch_type).into(),
                            merges,
                        }
                    })
                    .collect::<Vec<_>>();
                edges.sort_by_key(|edge| (edge.source, edge.target, edge.branch));
                edges
            })
        } else {
            Vec::new()
        };

        let locals = if wants_locals {
            crate::metrics::time(crate::metrics::Metric::FactsLocals, || {
                local_names
                    .into_iter()
                    .map(|local| {
                        let fact = LocalFact {
                            bindings: function.provenance().bindings(&local),
                            definitions: function.provenance().origins(&local),
                            uses: function.provenance().uses(&local),
                        };
                        (local, fact)
                    })
                    .collect()
            })
        } else {
            BTreeMap::new()
        };

        let dominators = if requested.contains(FactSet::DOMINATORS) {
            crate::metrics::time(crate::metrics::Metric::FactsDominators, || {
                derive_dominators(function)
            })
        } else {
            BTreeMap::new()
        };

        let post_dominators = if requested.contains(FactSet::POST_DOMINATORS) {
            crate::metrics::time(crate::metrics::Metric::FactsPostDominators, || {
                derive_post_dominators(function)
            })
        } else {
            BTreeMap::new()
        };

        let candidate_regions =
            crate::metrics::time(crate::metrics::Metric::FactsCandidateRegions, || {
                derive_candidate_regions(function)
            });

        Ok(Self {
            function_id: function.id,
            derived: requested,
            locals,
            statement_origins,
            edges,
            dominators,
            post_dominators,
            candidate_regions,
            effects,
        })
    }

    pub const fn function_id(&self) -> usize {
        self.function_id
    }

    /// Which optional facts this value carries.
    pub const fn derived(&self) -> FactSet {
        self.derived
    }

    /// Always available; never gated by [`FactSet`].
    pub fn candidate_regions(&self) -> &[CandidateRegion] {
        &self.candidate_regions
    }

    pub fn locals(&self) -> Option<&BTreeMap<RcLocal, LocalFact>> {
        self.derived
            .contains(FactSet::LOCALS)
            .then_some(&self.locals)
    }

    pub fn statement_origins(&self) -> Option<&BTreeMap<StatementLocation, OriginSet>> {
        self.derived
            .contains(FactSet::STATEMENT_ORIGINS)
            .then_some(&self.statement_origins)
    }

    pub fn edges(&self) -> Option<&[EdgeFact]> {
        self.derived
            .contains(FactSet::EDGES)
            .then_some(self.edges.as_slice())
    }

    pub fn dominators(&self) -> Option<&BTreeMap<NodeIndex, BTreeSet<NodeIndex>>> {
        self.derived
            .contains(FactSet::DOMINATORS)
            .then_some(&self.dominators)
    }

    pub fn post_dominators(&self) -> Option<&BTreeMap<NodeIndex, BTreeSet<NodeIndex>>> {
        self.derived
            .contains(FactSet::POST_DOMINATORS)
            .then_some(&self.post_dominators)
    }

    pub fn effects(&self) -> Option<&BTreeMap<StatementLocation, EffectSummary>> {
        self.derived
            .contains(FactSet::EFFECTS)
            .then_some(&self.effects)
    }
}

fn derive_dominators(function: &Function) -> BTreeMap<NodeIndex, BTreeSet<NodeIndex>> {
    let Some(entry) = *function.entry() else {
        return BTreeMap::new();
    };
    let dominators = simple_fast(function.graph(), entry);
    function
        .graph()
        .node_indices()
        .map(|node| {
            let values = dominators
                .dominators(node)
                .map(|values| values.collect())
                .unwrap_or_else(|| BTreeSet::from([node]));
            (node, values)
        })
        .collect()
}

fn derive_post_dominators(function: &Function) -> BTreeMap<NodeIndex, BTreeSet<NodeIndex>> {
    let nodes = function.graph().node_indices().collect::<BTreeSet<_>>();
    let mut facts = nodes
        .iter()
        .map(|&node| {
            let successors = function.successor_blocks(node).collect::<Vec<_>>();
            (
                node,
                if successors.is_empty() {
                    BTreeSet::from([node])
                } else {
                    nodes.clone()
                },
            )
        })
        .collect::<BTreeMap<_, _>>();

    loop {
        let mut changed = false;
        for &node in nodes.iter().rev() {
            let successors = function.successor_blocks(node).collect::<Vec<_>>();
            if successors.is_empty() {
                continue;
            }
            let mut intersection = facts[&successors[0]].clone();
            for successor in &successors[1..] {
                intersection = intersection
                    .intersection(&facts[successor])
                    .copied()
                    .collect();
            }
            intersection.insert(node);
            if facts[&node] != intersection {
                facts.insert(node, intersection);
                changed = true;
            }
        }
        if !changed {
            return facts;
        }
    }
}

fn derive_candidate_regions(function: &Function) -> Vec<CandidateRegion> {
    let mut regions = kosaraju_scc(function.graph())
        .into_iter()
        .filter(|members| {
            members.len() > 1
                || members.first().is_some_and(|&node| {
                    function
                        .graph()
                        .edges(node)
                        .any(|edge| edge.target() == node)
                })
        })
        .map(|members| {
            let members = members.into_iter().collect::<BTreeSet<_>>();
            let header = members
                .iter()
                .copied()
                .filter(|&node| {
                    function
                        .predecessor_blocks(node)
                        .any(|predecessor| !members.contains(&predecessor))
                })
                .min()
                .unwrap_or_else(|| *members.first().unwrap());
            CandidateRegion { header, members }
        })
        .collect::<Vec<_>>();
    regions.sort_by_key(|region| region.header);
    regions
}

fn summarize_effects(
    function: &Function,
    location: StatementLocation,
    statement: &Statement,
) -> EffectSummary {
    let mut summary = EffectSummary::default();
    for local in statement.values_read() {
        add_binding(
            function,
            local,
            &mut summary.local_reads,
            &mut summary.upvalue_reads,
        );
    }
    for local in statement.values_written() {
        add_binding(
            function,
            local,
            &mut summary.local_writes,
            &mut summary.upvalue_writes,
        );
    }

    match statement {
        Statement::Call(call) => {
            record_result(
                &mut summary,
                location,
                ResultProducer::Call,
                ResultShape::Exact(0),
            );
            summarize_call(function, &mut summary, &call.value, &call.arguments);
        }
        Statement::MethodCall(call) => {
            record_result(
                &mut summary,
                location,
                ResultProducer::MethodCall,
                ResultShape::Exact(0),
            );
            summarize_method_call(function, &mut summary, &call.value, &call.arguments);
        }
        Statement::Assign(assign) | Statement::GenericForInit(ast::GenericForInit(assign)) => {
            for lvalue in &assign.left {
                summarize_lvalue(function, &mut summary, lvalue);
            }
            for (index, value) in assign.right.iter().enumerate() {
                let demand = if index + 1 == assign.right.len() {
                    ResultShape::Exact(assign.left.len().saturating_sub(index))
                } else {
                    ResultShape::Exact(1)
                };
                summarize_rvalue(function, location, &mut summary, value, demand);
            }
        }
        Statement::Return(return_) => {
            for (index, value) in return_.values.iter().enumerate() {
                let demand = if index + 1 == return_.values.len() {
                    ResultShape::Open
                } else {
                    ResultShape::Exact(1)
                };
                summarize_rvalue(function, location, &mut summary, value, demand);
                add_root_binding(function, value, &mut summary.table_root_escapes);
            }
        }
        Statement::SetList(set_list) => {
            add_local_bindings(
                function,
                &set_list.object_local,
                &mut summary.table_root_writes,
            );
            for value in &set_list.values {
                summarize_rvalue(
                    function,
                    location,
                    &mut summary,
                    value,
                    ResultShape::Exact(1),
                );
            }
            if let Some(tail) = &set_list.tail {
                summarize_rvalue(function, location, &mut summary, tail, ResultShape::Open);
            }
        }
        Statement::GenericForNext(next) => {
            record_result(
                &mut summary,
                location,
                ResultProducer::GeneratorCall,
                ResultShape::Exact(next.res_locals.len()),
            );
            summary.calls = true;
            summarize_rvalue(
                function,
                location,
                &mut summary,
                &next.generator,
                ResultShape::Exact(1),
            );
            summarize_rvalue(
                function,
                location,
                &mut summary,
                &next.state,
                ResultShape::Exact(1),
            );
        }
        _ => {
            for value in ast::Traverse::rvalues(statement) {
                summarize_rvalue(
                    function,
                    location,
                    &mut summary,
                    value,
                    ResultShape::Exact(1),
                );
            }
        }
    }
    summary
}

fn add_binding(
    function: &Function,
    local: &RcLocal,
    locals: &mut BTreeSet<BindingIdentity>,
    upvalues: &mut BTreeSet<BindingIdentity>,
) {
    for binding in function.provenance().bindings(local) {
        if matches!(binding, BindingIdentity::Upvalue { .. }) {
            upvalues.insert(binding);
        } else {
            locals.insert(binding);
        }
    }
}

fn add_local_bindings(
    function: &Function,
    local: &RcLocal,
    output: &mut BTreeSet<BindingIdentity>,
) {
    output.extend(function.provenance().bindings(local));
}

fn root_local(value: &RValue) -> Option<&RcLocal> {
    match value {
        RValue::Local(local) => Some(local),
        RValue::Index(index) => root_local(&index.left),
        _ => None,
    }
}

fn add_root_binding(function: &Function, value: &RValue, output: &mut BTreeSet<BindingIdentity>) {
    if let Some(local) = root_local(value) {
        add_local_bindings(function, local, output);
    }
}

fn summarize_lvalue(function: &Function, summary: &mut EffectSummary, value: &LValue) {
    if let LValue::Index(index) = value {
        summary.metamethod_capable = true;
        add_root_binding(function, &index.left, &mut summary.table_root_writes);
    }
}

fn record_result(
    summary: &mut EffectSummary,
    location: StatementLocation,
    producer: ResultProducer,
    demand: ResultShape,
) {
    summary.result_groups.push(ResultGroupFact {
        location,
        ordinal: summary.result_groups.len(),
        producer,
        demand,
    });
}

fn summarize_call(
    function: &Function,
    summary: &mut EffectSummary,
    callee: &RValue,
    arguments: &[RValue],
) {
    summary.calls = true;
    summarize_nested(function, summary, callee, ResultShape::Exact(1));
    for (index, argument) in arguments.iter().enumerate() {
        let demand = if index + 1 == arguments.len() {
            ResultShape::Open
        } else {
            ResultShape::Exact(1)
        };
        summarize_nested(function, summary, argument, demand);
        add_root_binding(function, argument, &mut summary.table_root_escapes);
    }
}

fn summarize_method_call(
    function: &Function,
    summary: &mut EffectSummary,
    receiver: &RValue,
    arguments: &[RValue],
) {
    summary.calls = true;
    summary.metamethod_capable = true;
    add_root_binding(function, receiver, &mut summary.table_root_reads);
    summarize_nested(function, summary, receiver, ResultShape::Exact(1));
    for (index, argument) in arguments.iter().enumerate() {
        let demand = if index + 1 == arguments.len() {
            ResultShape::Open
        } else {
            ResultShape::Exact(1)
        };
        summarize_nested(function, summary, argument, demand);
        add_root_binding(function, argument, &mut summary.table_root_escapes);
    }
}

fn summarize_nested(
    function: &Function,
    summary: &mut EffectSummary,
    value: &RValue,
    demand: ResultShape,
) {
    let location = summary
        .result_groups
        .last()
        .map(|group| group.location)
        .unwrap_or(StatementLocation {
            block: NodeIndex::new(usize::MAX),
            statement: usize::MAX,
        });
    summarize_rvalue(function, location, summary, value, demand);
}

fn summarize_rvalue(
    function: &Function,
    location: StatementLocation,
    summary: &mut EffectSummary,
    value: &RValue,
    demand: ResultShape,
) {
    match value {
        RValue::Call(call) => {
            record_result(summary, location, ResultProducer::Call, demand);
            summarize_call(function, summary, &call.value, &call.arguments);
        }
        RValue::MethodCall(call) => {
            record_result(summary, location, ResultProducer::MethodCall, demand);
            summarize_method_call(function, summary, &call.value, &call.arguments);
        }
        RValue::VarArg(_) => record_result(summary, location, ResultProducer::VarArg, demand),
        RValue::Select(select) => match select {
            ast::Select::Call(call) => {
                record_result(
                    summary,
                    location,
                    ResultProducer::Call,
                    ResultShape::Exact(1),
                );
                summarize_call(function, summary, &call.value, &call.arguments);
            }
            ast::Select::MethodCall(call) => {
                record_result(
                    summary,
                    location,
                    ResultProducer::MethodCall,
                    ResultShape::Exact(1),
                );
                summarize_method_call(function, summary, &call.value, &call.arguments);
            }
            ast::Select::VarArg(_) => record_result(
                summary,
                location,
                ResultProducer::VarArg,
                ResultShape::Exact(1),
            ),
        },
        RValue::Table(table) => {
            summary.allocation = true;
            for (index, (key, value)) in table.0.iter().enumerate() {
                if let Some(key) = key {
                    summarize_rvalue(function, location, summary, key, ResultShape::Exact(1));
                }
                let demand = if key.is_none() && index + 1 == table.0.len() {
                    ResultShape::Open
                } else {
                    ResultShape::Exact(1)
                };
                summarize_rvalue(function, location, summary, value, demand);
            }
        }
        RValue::Closure(closure) => {
            summary.allocation = true;
            summary.closure_capture |= !closure.upvalues.is_empty();
        }
        RValue::Index(index) => {
            summary.metamethod_capable = true;
            add_root_binding(function, &index.left, &mut summary.table_root_reads);
            summarize_rvalue(
                function,
                location,
                summary,
                &index.left,
                ResultShape::Exact(1),
            );
            summarize_rvalue(
                function,
                location,
                summary,
                &index.right,
                ResultShape::Exact(1),
            );
        }
        RValue::Binary(binary) => {
            summary.metamethod_capable |= !matches!(
                binary.operation,
                ast::BinaryOperation::And | ast::BinaryOperation::Or
            );
            summarize_rvalue(
                function,
                location,
                summary,
                &binary.left,
                ResultShape::Exact(1),
            );
            summarize_rvalue(
                function,
                location,
                summary,
                &binary.right,
                ResultShape::Exact(1),
            );
        }
        RValue::Unary(unary) => {
            summary.metamethod_capable |= !matches!(unary.operation, ast::UnaryOperation::Not);
            summarize_rvalue(
                function,
                location,
                summary,
                &unary.value,
                ResultShape::Exact(1),
            );
        }
        RValue::Conditional(conditional) => {
            for nested in [
                conditional.condition.as_ref(),
                conditional.then_value.as_ref(),
                conditional.else_value.as_ref(),
            ] {
                summarize_rvalue(function, location, summary, nested, ResultShape::Exact(1));
            }
        }
        RValue::Local(_) | RValue::Global(_) | RValue::Literal(_) => {}
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct PassChange(u8);

impl PassChange {
    const DATAFLOW: u8 = 1 << 0;
    const CFG: u8 = 1 << 1;
    const REGIONS: u8 = 1 << 2;
    const AST: u8 = 1 << 3;

    pub const fn none() -> Self {
        Self(0)
    }

    pub const fn dataflow() -> Self {
        Self(Self::DATAFLOW)
    }

    pub const fn cfg() -> Self {
        Self(Self::CFG)
    }

    pub const fn regions() -> Self {
        Self(Self::REGIONS)
    }

    pub const fn ast() -> Self {
        Self(Self::AST)
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn invalidates_dataflow(self) -> bool {
        self.0 & (Self::DATAFLOW | Self::CFG | Self::AST) != 0
    }

    pub const fn invalidates_cfg(self) -> bool {
        self.0 & Self::CFG != 0
    }

    pub const fn invalidates_regions(self) -> bool {
        self.0 & (Self::CFG | Self::REGIONS) != 0
    }

    pub const fn invalidates_ast(self) -> bool {
        self.0 & Self::AST != 0
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InvalidationCounts {
    pub dataflow: usize,
    pub cfg: usize,
    pub regions: usize,
    pub ast: usize,
}

impl InvalidationCounts {
    fn record(&mut self, change: PassChange) {
        self.dataflow += usize::from(change.invalidates_dataflow());
        self.cfg += usize::from(change.invalidates_cfg());
        self.regions += usize::from(change.invalidates_regions());
        self.ast += usize::from(change.invalidates_ast());
    }
}

type PassOperation<'a> = dyn FnMut(&mut Function) -> PassChange + 'a;

struct ScheduledPass<'a> {
    name: &'static str,
    operation: Box<PassOperation<'a>>,
}

pub struct PassScheduler<'a> {
    max_rounds: usize,
    passes: Vec<ScheduledPass<'a>>,
}

impl<'a> PassScheduler<'a> {
    pub fn new(max_rounds: usize) -> Self {
        assert!(max_rounds > 0, "scheduler must allow at least one round");
        Self {
            max_rounds,
            passes: Vec::new(),
        }
    }

    pub fn add_pass(
        &mut self,
        name: &'static str,
        operation: impl FnMut(&mut Function) -> PassChange + 'a,
    ) {
        self.passes.push(ScheduledPass {
            name,
            operation: Box::new(operation),
        });
    }

    pub fn run(&mut self, function: &mut Function) -> Result<SchedulerReport, SchedulerError> {
        crate::metrics::record_scheduler_run();
        let mut facts =
            crate::metrics::time(crate::metrics::Metric::FactsDerive, || {
                RecoveryFacts::derive_subset(function, FactSet::RECONSTRUCTION)
            })?;
        let mut invalidations = InvalidationCounts::default();
        let initial_fingerprint = crate::metrics::time(crate::metrics::Metric::Fingerprint, || {
            structural_fingerprint(function)
        });
        let mut seen = HashMap::from([(initial_fingerprint, 0usize)]);
        let mut applied_changes = Vec::new();

        for round in 1..=self.max_rounds {
            crate::metrics::record_round();
            let mut round_change = PassChange::none();
            for pass in &mut self.passes {
                let change = (pass.operation)(function);
                if !change.is_empty() {
                    invalidations.record(change);
                    round_change = round_change.union(change);
                    applied_changes.push((pass.name, change));
                }
            }

            if round_change.is_empty() {
                return Ok(SchedulerReport {
                    facts,
                    rounds: round,
                    applied_changes,
                    invalidations,
                });
            }

            facts = crate::metrics::time(crate::metrics::Metric::FactsDerive, || {
                RecoveryFacts::derive_subset(function, FactSet::RECONSTRUCTION)
            })?;
            let fingerprint = crate::metrics::time(crate::metrics::Metric::Fingerprint, || {
                structural_fingerprint(function)
            });
            if let Some(first_round) = seen.insert(fingerprint, round) {
                return Err(SchedulerError::RepeatedState {
                    function_id: function.id,
                    first_round,
                    repeated_round: round,
                    changed_passes: applied_changes
                        .iter()
                        .rev()
                        .take(self.passes.len())
                        .map(|(name, _)| *name)
                        .collect(),
                });
            }
        }

        Err(SchedulerError::RoundLimit {
            function_id: function.id,
            max_rounds: self.max_rounds,
            changed_passes: applied_changes
                .iter()
                .rev()
                .take(self.passes.len())
                .map(|(name, _)| *name)
                .collect(),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SchedulerReport {
    pub facts: RecoveryFacts,
    pub rounds: usize,
    pub applied_changes: Vec<(&'static str, PassChange)>,
    pub invalidations: InvalidationCounts,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SchedulerError {
    #[error(transparent)]
    Recovery(#[from] RecoveryError),
    #[error(
        "function {function_id} reconstruction repeated round {first_round} at round {repeated_round}; changed passes: {changed_passes:?}"
    )]
    RepeatedState {
        function_id: usize,
        first_round: usize,
        repeated_round: usize,
        changed_passes: Vec<&'static str>,
    },
    #[error(
        "function {function_id} reconstruction exceeded {max_rounds} rounds; changed passes: {changed_passes:?}"
    )]
    RoundLimit {
        function_id: usize,
        max_rounds: usize,
        changed_passes: Vec<&'static str>,
    },
}

/// Feeds formatted output straight into a hasher.
///
/// The fingerprint covers whole block contents, so materializing the
/// rendering first meant building — and repeatedly reallocating — a string
/// proportional to the function's entire AST on every call.
struct HashWriter<'a, H: Hasher>(&'a mut H);

impl<H: Hasher> fmt::Write for HashWriter<'_, H> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0.write(text.as_bytes());
        Ok(())
    }
}

/// Hashes the function's structure and contents.
///
/// The value is compared only against other fingerprints from the same run,
/// to detect a reconstruction round that reproduced an earlier state. It is
/// never persisted, so only the equality relation matters, not the value.
pub fn structural_fingerprint(function: &Function) -> u64 {
    use fmt::Write;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut sink = HashWriter(&mut hasher);

    let _ = write!(sink, "entry={:?};", function.entry());
    for (node, block) in function.blocks() {
        let _ = write!(sink, "node={}:block={block:?};", node.index());
    }

    // Edge order is not stable across graph mutations, so the rendered edges
    // are sorted before hashing. Sorting requires them materialized.
    let mut edges = function
        .graph()
        .edge_references()
        .map(|edge| {
            (
                edge.source().index(),
                edge.target().index(),
                BranchKind::from(&edge.weight().branch_type),
                format!("{:?}", edge.weight().arguments),
            )
        })
        .collect::<Vec<_>>();
    edges.sort();
    let _ = write!(sink, "edges={edges:?};");

    hasher.finish()
}

#[cfg(test)]
mod tests {
    use ast::{Assign, Binary, BinaryOperation, Call, Global, LValue, Literal, Local, RValue};

    use super::{
        PassChange, PassScheduler, RecoveryFacts, ResultShape, SchedulerError, StatementLocation,
    };
    use crate::{
        block::{BlockEdge, BranchType},
        function::Function,
        provenance::{BindingIdentity, SourceOrigin},
    };

    fn local(name: &str) -> ast::RcLocal {
        ast::RcLocal::new(Local::new(Some(name.to_owned())))
    }

    #[test]
    fn recovery_facts_capture_edges_merges_and_effects() {
        let mut function = Function::new(7);
        let source = function.new_block();
        let target = function.new_block();
        function.set_entry(source);
        let table = local("table");
        let result = local("result");
        function.set_binding(table.clone(), BindingIdentity::local(7, 0));
        function.set_binding(result.clone(), BindingIdentity::local(7, 1));
        function.block_mut(source).unwrap().push(
            Assign::new(
                vec![LValue::Local(result.clone())],
                vec![Call::new(Global::from("next").into(), vec![table.clone().into()]).into()],
            )
            .into(),
        );
        function.set_statement_origins(
            source,
            vec![std::collections::BTreeSet::from([SourceOrigin::new(
                7,
                2,
                Some(3),
                "CALL",
            )])],
        );
        function.graph_mut().add_edge(
            source,
            target,
            BlockEdge {
                branch_type: BranchType::Unconditional,
                arguments: vec![(result.clone(), RValue::Local(result.clone()))],
            },
        );

        let facts = RecoveryFacts::derive(&function).unwrap();
        let location = StatementLocation {
            block: source,
            statement: 0,
        };
        assert_eq!(facts.function_id(), 7);
        assert_eq!(facts.edges.len(), 1);
        assert_eq!(facts.edges[0].merges.len(), 1);
        assert!(facts.effects[&location].calls);
        assert_eq!(
            facts.effects[&location].result_groups[0].demand,
            ResultShape::Exact(1)
        );
        assert_eq!(facts.statement_origins[&location].len(), 1);
    }

    #[test]
    fn effect_summary_marks_metamethod_capable_arithmetic() {
        let mut function = Function::new(8);
        let block = function.new_block();
        function.set_entry(block);
        let left = local("left");
        let result = local("result");
        function.set_binding(left.clone(), BindingIdentity::local(8, 0));
        function.set_binding(result.clone(), BindingIdentity::local(8, 1));
        function.block_mut(block).unwrap().push(
            Assign::new(
                vec![result.into()],
                vec![
                    Binary::new(
                        left.into(),
                        Literal::Integer(1).into(),
                        BinaryOperation::Add,
                    )
                    .into(),
                ],
            )
            .into(),
        );

        let facts = RecoveryFacts::derive(&function).unwrap();
        assert!(
            facts.effects[&StatementLocation {
                block,
                statement: 0
            }]
                .metamethod_capable
        );
    }

    #[test]
    fn no_change_scheduler_run_is_idempotent() {
        let mut function = Function::new(8);
        let mut scheduler = PassScheduler::new(4);
        scheduler.add_pass("noop", |_| PassChange::none());

        let first = scheduler.run(&mut function).unwrap();
        let second = scheduler.run(&mut function).unwrap();
        assert_eq!(first.facts, second.facts);
        assert_eq!(first.rounds, 1);
        assert_eq!(second.rounds, 1);
    }

    #[test]
    fn changed_cfg_and_dataflow_invalidate_only_dependent_facts() {
        let change = PassChange::cfg().union(PassChange::dataflow());
        assert!(change.invalidates_cfg());
        assert!(change.invalidates_dataflow());
        assert!(change.invalidates_regions());
        assert!(!change.invalidates_ast());
    }

    #[test]
    fn scheduler_reports_a_repeated_state_instead_of_spinning() {
        let mut function = Function::new(9);
        let block = function.new_block();
        function.set_entry(block);
        let mut present = false;
        let mut scheduler = PassScheduler::new(8);
        scheduler.add_pass("oscillate", move |function| {
            if present {
                function.block_mut(block).unwrap().pop();
            } else {
                function
                    .block_mut(block)
                    .unwrap()
                    .push(ast::Empty {}.into());
            }
            present = !present;
            PassChange::ast()
        });

        let error = scheduler.run(&mut function).unwrap_err();
        assert!(matches!(error, SchedulerError::RepeatedState { .. }));
    }
}
