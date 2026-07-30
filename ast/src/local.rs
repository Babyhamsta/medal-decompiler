use smallvec::{SmallVec, smallvec};
use crate::{LocalRefs,LocalRefsMut};
use crate::{SideEffects, Traverse, Type, TypeSystem, type_system::Infer};
use by_address::ByAddress;
use derive_more::From;
use enum_dispatch::enum_dispatch;
use parking_lot::Mutex;
use std::{
    fmt::{self, Display},
    hash::{Hash, Hasher},
    sync::atomic::{AtomicU64, Ordering},
};
use triomphe::Arc;

#[derive(Debug, Default, From, Clone, PartialEq, PartialOrd, Ord, Eq, Hash)]
pub struct Local(pub Option<String>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentifierContext {
    Local,
    Parameter,
    GlobalExpression,
    FunctionName,
    MethodName,
    MemberName,
    TableField,
    TypeDeclaration,
}

pub fn is_valid_identifier_in(name: &[u8], context: IdentifierContext) -> bool {
    if name.is_empty()
        || !name.iter().enumerate().all(|(index, character)| {
            (index != 0 && character.is_ascii_digit())
                || character.is_ascii_alphabetic()
                || *character == b'_'
        })
    {
        return false;
    }

    const HARD_KEYWORDS: &[&str] = &[
        "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "if", "in",
        "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
    ];

    let hard_keywords = match context {
        IdentifierContext::Local
        | IdentifierContext::Parameter
        | IdentifierContext::GlobalExpression
        | IdentifierContext::FunctionName
        | IdentifierContext::MethodName
        | IdentifierContext::MemberName
        | IdentifierContext::TableField
        | IdentifierContext::TypeDeclaration => HARD_KEYWORDS,
    };
    std::str::from_utf8(name)
        .ok()
        .is_some_and(|name| !hard_keywords.contains(&name))
}

pub fn is_valid_identifier(name: &[u8]) -> bool {
    is_valid_identifier_in(name, IdentifierContext::Local)
}

impl Local {
    pub fn new(name: Option<String>) -> Self {
        Self(name)
    }
}

impl fmt::Display for Local {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.0 {
            Some(name) => write!(f, "{}", name),
            None => write!(f, "UNNAMED_LOCAL"),
        }
    }
}

static NEXT_LOCAL_ID: AtomicU64 = AtomicU64::new(0);

fn next_local_id() -> u64 {
    NEXT_LOCAL_ID.fetch_add(1, Ordering::Relaxed)
}

/// A local's identity is its creation sequence number, not its heap address.
///
/// Locals are hashed, compared, and ordered throughout the pipeline, and
/// several passes iterate collections keyed by them — `BTreeSet<RcLocal>` in
/// `local_declarations` drives declaration placement. Deriving those traits
/// from the address made iteration follow allocator placement, so the same
/// input decompiled to a different byte stream on each run.
///
/// A creation counter preserves the identity relation exactly: one id per
/// allocation, shared by clones, never reused. Construction is confined to
/// `new` and `default`, so no path can pair an existing allocation with a
/// fresh id.
#[derive(Debug, Clone)]
pub struct RcLocal(pub ByAddress<Arc<Mutex<Local>>>, u64);

impl Default for RcLocal {
    fn default() -> Self {
        Self::new(Local::default())
    }
}

impl PartialEq for RcLocal {
    fn eq(&self, other: &Self) -> bool {
        self.1 == other.1
    }
}

impl Eq for RcLocal {}

impl PartialOrd for RcLocal {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RcLocal {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.1.cmp(&other.1)
    }
}

impl Hash for RcLocal {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.1.hash(state);
    }
}

impl Infer for RcLocal {
    fn infer<'a: 'b, 'b>(&'a mut self, system: &mut TypeSystem<'b>) -> Type {
        system.type_of(self).clone()
    }
}

impl Display for RcLocal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0.0.lock().0 {
            Some(name) => write!(f, "{}", name),
            None => write!(f, "UNNAMED_{}", self.1),
        }
    }
}

impl SideEffects for RcLocal {}

impl Traverse for RcLocal {}

impl RcLocal {
    pub fn new(local: Local) -> Self {
        Self(ByAddress(Arc::new(Mutex::new(local))), next_local_id())
    }

    /// The creation sequence number that gives this local its identity.
    pub const fn id(&self) -> u64 {
        self.1
    }
}

impl LocalRw for RcLocal {
    fn values_read(&self) -> LocalRefs<'_> {
        smallvec![self]
    }

    fn values_read_mut(&mut self) -> LocalRefsMut<'_> {
        smallvec![self]
    }
}

#[cfg(test)]
mod identifier_tests {
    use super::{IdentifierContext, is_valid_identifier_in};

    #[test]
    fn contextual_words_are_legal_identifiers_in_supported_luau_contexts() {
        for context in [
            IdentifierContext::Local,
            IdentifierContext::Parameter,
            IdentifierContext::GlobalExpression,
            IdentifierContext::FunctionName,
            IdentifierContext::MethodName,
            IdentifierContext::MemberName,
            IdentifierContext::TableField,
            IdentifierContext::TypeDeclaration,
        ] {
            for name in [
                b"type".as_slice(),
                b"class",
                b"continue",
                b"export",
                b"goto",
            ] {
                assert!(
                    is_valid_identifier_in(name, context),
                    "{name:?} in {context:?}"
                );
            }
        }
    }

    #[test]
    fn hard_keywords_and_invalid_shapes_stay_illegal() {
        for context in [
            IdentifierContext::Local,
            IdentifierContext::GlobalExpression,
            IdentifierContext::MethodName,
            IdentifierContext::TableField,
        ] {
            assert!(!is_valid_identifier_in(b"end", context));
            assert!(!is_valid_identifier_in(b"bad-name", context));
            assert!(!is_valid_identifier_in(b"2fast", context));
        }
    }
}

#[enum_dispatch]
pub trait LocalRw {
    fn values_read(&self) -> LocalRefs<'_> {
        SmallVec::new()
    }

    fn values_read_mut(&mut self) -> LocalRefsMut<'_> {
        SmallVec::new()
    }

    fn values_written(&self) -> LocalRefs<'_> {
        SmallVec::new()
    }

    fn values_written_mut(&mut self) -> LocalRefsMut<'_> {
        SmallVec::new()
    }

    fn values(&self) -> LocalRefs<'_> {
        self.values_read()
            .into_iter()
            .chain(self.values_written())
            .collect()
    }

    fn replace_values_read(&mut self, old: &RcLocal, new: &RcLocal) {
        for value in self.values_read_mut() {
            if value == old {
                *value = new.clone();
            }
        }
    }

    fn replace_values_written(&mut self, old: &RcLocal, new: &RcLocal) {
        for value in self.values_written_mut() {
            if value == old {
                *value = new.clone();
            }
        }
    }

    fn replace_values(&mut self, old: &RcLocal, new: &RcLocal) {
        self.replace_values_read(old, new);
        self.replace_values_written(old, new);
    }
}
