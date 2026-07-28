use std::collections::BTreeSet;

use ast::RcLocal;
use rustc_hash::FxHashMap;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum BindingIdentity {
    Local {
        function_id: usize,
        register: usize,
        lifetime: Option<(usize, usize)>,
    },
    SyntheticLocal {
        function_id: usize,
        sequence: usize,
    },
    Parameter {
        function_id: usize,
        register: usize,
    },
    Upvalue {
        function_id: usize,
        index: usize,
    },
    Global(Vec<u8>),
    Import(Vec<u8>),
    Member(Vec<u8>),
}

impl BindingIdentity {
    pub const fn local(function_id: usize, register: usize) -> Self {
        Self::Local {
            function_id,
            register,
            lifetime: None,
        }
    }

    pub const fn parameter(function_id: usize, register: usize) -> Self {
        Self::Parameter {
            function_id,
            register,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DebugLifetime {
    pub name: Vec<u8>,
    pub start_instruction: usize,
    pub end_instruction: usize,
}

impl DebugLifetime {
    pub const fn new(name: Vec<u8>, start_instruction: usize, end_instruction: usize) -> Self {
        Self {
            name,
            start_instruction,
            end_instruction,
        }
    }

    pub const fn contains(&self, instruction: usize) -> bool {
        self.start_instruction <= instruction && instruction < self.end_instruction
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CaptureOrigin {
    pub closure_function_id: usize,
    pub capture_index: usize,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SourceOrigin {
    pub function_id: usize,
    pub instruction: usize,
    pub source_line: Option<usize>,
    pub opcode: String,
    pub register_family: Option<usize>,
    pub debug_lifetime: Option<DebugLifetime>,
    pub capture: Option<CaptureOrigin>,
}

impl SourceOrigin {
    pub fn new(
        function_id: usize,
        instruction: usize,
        source_line: Option<usize>,
        opcode: impl Into<String>,
    ) -> Self {
        Self {
            function_id,
            instruction,
            source_line,
            opcode: opcode.into(),
            register_family: None,
            debug_lifetime: None,
            capture: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisterFamily {
    pub function_id: usize,
    pub register: usize,
    pub binding: BindingIdentity,
    pub debug_lifetimes: Vec<DebugLifetime>,
}

impl RegisterFamily {
    pub fn new(
        function_id: usize,
        register: usize,
        binding: BindingIdentity,
        mut debug_lifetimes: Vec<DebugLifetime>,
    ) -> Self {
        debug_lifetimes
            .sort_by_key(|lifetime| (lifetime.start_instruction, lifetime.end_instruction));
        Self {
            function_id,
            register,
            binding,
            debug_lifetimes,
        }
    }

    pub fn debug_lifetime_at(&self, instruction: usize) -> Option<&DebugLifetime> {
        let mut matching = self
            .debug_lifetimes
            .iter()
            .filter(|lifetime| lifetime.contains(instruction));
        let lifetime = matching.next()?;
        matching.next().is_none().then_some(lifetime)
    }

    pub fn debug_name_at(&self, instruction: usize) -> Option<&[u8]> {
        self.debug_lifetime_at(instruction)
            .map(|lifetime| lifetime.name.as_slice())
    }

    pub fn binding_at(&self, instruction: usize) -> BindingIdentity {
        match (&self.binding, self.debug_lifetime_at(instruction)) {
            (
                BindingIdentity::Local {
                    function_id,
                    register,
                    ..
                },
                Some(lifetime),
            ) => BindingIdentity::Local {
                function_id: *function_id,
                register: *register,
                lifetime: Some((lifetime.start_instruction, lifetime.end_instruction)),
            },
            _ => self.binding.clone(),
        }
    }

    pub fn definition_origin(&self, mut origin: SourceOrigin) -> SourceOrigin {
        origin.register_family = Some(self.register);
        origin.debug_lifetime = self.debug_lifetime_at(origin.instruction).cloned();
        origin
    }
}

pub type OriginSet = BTreeSet<SourceOrigin>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalProvenance {
    bindings: BTreeSet<BindingIdentity>,
    definitions: OriginSet,
    uses: OriginSet,
}

#[derive(Clone, Debug, Default)]
pub struct Provenance {
    locals: FxHashMap<RcLocal, LocalProvenance>,
}

impl Provenance {
    pub fn ensure_local(&mut self, local: RcLocal, binding: BindingIdentity) {
        self.locals.entry(local).or_insert_with(|| LocalProvenance {
            bindings: BTreeSet::from([binding]),
            definitions: OriginSet::new(),
            uses: OriginSet::new(),
        });
    }

    pub fn record_definition(
        &mut self,
        local: RcLocal,
        binding: BindingIdentity,
        origin: SourceOrigin,
    ) {
        let entry = self.locals.entry(local).or_insert_with(|| LocalProvenance {
            bindings: BTreeSet::new(),
            definitions: OriginSet::new(),
            uses: OriginSet::new(),
        });
        entry.bindings.insert(binding);
        entry.definitions.insert(origin);
    }

    pub fn record_use(&mut self, local: RcLocal, origin: SourceOrigin) {
        if let Some(entry) = self.locals.get_mut(&local) {
            entry.uses.insert(origin);
        }
    }

    pub fn merge_locals<'a>(
        &mut self,
        target: RcLocal,
        fallback_binding: BindingIdentity,
        sources: impl IntoIterator<Item = &'a RcLocal>,
    ) {
        let mut definitions = OriginSet::new();
        let mut uses = OriginSet::new();
        let mut bindings = BTreeSet::new();
        for source in sources {
            if let Some(provenance) = self.locals.get(source) {
                bindings.extend(provenance.bindings.iter().cloned());
                definitions.extend(provenance.definitions.iter().cloned());
                uses.extend(provenance.uses.iter().cloned());
            }
        }
        if bindings.is_empty() {
            bindings.insert(fallback_binding);
        }
        self.locals.insert(
            target,
            LocalProvenance {
                bindings,
                definitions,
                uses,
            },
        );
    }

    pub fn derive_local<'a>(
        &mut self,
        target: RcLocal,
        binding: BindingIdentity,
        sources: impl IntoIterator<Item = &'a RcLocal>,
    ) {
        let mut definitions = OriginSet::new();
        let mut uses = OriginSet::new();
        for source in sources {
            if let Some(provenance) = self.locals.get(source) {
                definitions.extend(provenance.definitions.iter().cloned());
                uses.extend(provenance.uses.iter().cloned());
            }
        }
        self.locals.insert(
            target,
            LocalProvenance {
                bindings: BTreeSet::from([binding]),
                definitions,
                uses,
            },
        );
    }

    pub fn origins(&self, local: &RcLocal) -> OriginSet {
        self.locals
            .get(local)
            .map(|provenance| provenance.definitions.clone())
            .unwrap_or_default()
    }

    pub fn uses(&self, local: &RcLocal) -> OriginSet {
        self.locals
            .get(local)
            .map(|provenance| provenance.uses.clone())
            .unwrap_or_default()
    }

    pub fn binding(&self, local: &RcLocal) -> Option<&BindingIdentity> {
        self.locals.get(local).and_then(|provenance| {
            (provenance.bindings.len() == 1).then(|| provenance.bindings.first().unwrap())
        })
    }

    pub fn bindings(&self, local: &RcLocal) -> BTreeSet<BindingIdentity> {
        self.locals
            .get(local)
            .map(|provenance| provenance.bindings.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use ast::{Local, RcLocal};

    use super::{BindingIdentity, DebugLifetime, Provenance, RegisterFamily, SourceOrigin};

    #[test]
    fn reused_register_definitions_keep_disjoint_debug_lifetimes() {
        let family = RegisterFamily::new(
            3,
            2,
            BindingIdentity::local(3, 2),
            vec![
                DebugLifetime::new(b"first".to_vec(), 1, 4),
                DebugLifetime::new(b"second".to_vec(), 7, 11),
            ],
        );

        let first = family.definition_origin(SourceOrigin::new(3, 2, Some(18), "LOADN"));
        let second = family.definition_origin(SourceOrigin::new(3, 8, Some(24), "LOADK"));

        assert_eq!(first.debug_lifetime.unwrap().name, b"first");
        assert_eq!(second.debug_lifetime.unwrap().name, b"second");
        assert_eq!(family.debug_name_at(5), None);
    }

    #[test]
    fn merge_retains_every_contributing_origin_and_binding() {
        let first = RcLocal::new(Local::new(Some("first".to_owned())));
        let second = RcLocal::new(Local::new(Some("second".to_owned())));
        let merged = RcLocal::default();
        let mut provenance = Provenance::default();

        provenance.record_definition(
            first.clone(),
            BindingIdentity::local(0, 1),
            SourceOrigin::new(0, 4, Some(8), "LOADN"),
        );
        provenance.record_definition(
            second.clone(),
            BindingIdentity::local(0, 1),
            SourceOrigin::new(0, 9, Some(12), "LOADK"),
        );
        provenance.merge_locals(
            merged.clone(),
            BindingIdentity::local(0, 1),
            [&first, &second],
        );

        assert_eq!(provenance.origins(&merged).len(), 2);
        assert_eq!(
            provenance.binding(&merged),
            Some(&BindingIdentity::local(0, 1))
        );
    }
}
