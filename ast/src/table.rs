use crate::{LocalRefs,LocalRefsMut,RValueRefs,RValueRefsMut};
use crate::{
    Literal, LocalRw, RValue, RcLocal, Reduce, SideEffects, Traverse, formatter::Formatter,
};

use std::{fmt, iter};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Table(pub Vec<(Option<RValue>, RValue)>);

impl Reduce for Table {
    fn reduce(self) -> RValue {
        self.into()
    }

    fn reduce_condition(self) -> RValue {
        if self.has_side_effects() {
            // TODO: remove all members w/o side effects
            self.into()
        } else {
            Literal::Boolean(true).into()
        }
    }
}

/*impl Infer for Table {
    fn infer<'a: 'b, 'b>(&'a mut self, system: &mut TypeSystem<'b>) -> Type {
        let elements: BTreeSet<_> = self
            .0
            .iter_mut()
            .map(|(f, v)| (f.clone(), v.infer(system)))
            .collect();
        let elements: BTreeSet<_> = elements
            .iter()
            .filter(|(f, t)| {
                f.is_some() || !elements.iter().any(|(_, x)| t != x && t.is_subtype_of(x))
            })
            .cloned()
            .collect();
        let (elements, fields): (BTreeSet<_>, BTreeMap<_, _>) =
            elements.into_iter().partition_map(|(f, t)| match f {
                None => Either::Left(t),
                Some(f) => Either::Right((f, t)),
            });

        Type::Table {
            indexer: Box::new((
                Type::Any,
                if elements.len() > 1 {
                    Type::Union(elements)
                } else {
                    elements.into_iter().next().unwrap_or(Type::Any)
                },
            )),
            fields,
        }
    }
}*/

impl LocalRw for Table {
    fn values_read(&self) -> LocalRefs<'_> {
        self.0
            .iter()
            .flat_map(|(k, v)| k.iter().chain(iter::once(v)))
            .flat_map(|v| v.values_read())
            .collect()
    }

    fn values_read_mut(&mut self) -> LocalRefsMut<'_> {
        self.0
            .iter_mut()
            .flat_map(|(k, v)| k.iter_mut().chain(iter::once(v)))
            .flat_map(|v| v.values_read_mut())
            .collect()
    }
}

impl Traverse for Table {
    fn rvalues_mut(&mut self) -> RValueRefsMut<'_> {
        self.0
            .iter_mut()
            .flat_map(|(k, v)| k.iter_mut().chain(iter::once(v)))
            .collect()
    }

    fn rvalues(&self) -> RValueRefs<'_> {
        self.0
            .iter()
            .flat_map(|(k, v)| k.iter().chain(iter::once(v)))
            .collect()
    }
}

impl SideEffects for Table {
    fn has_side_effects(&self) -> bool {
        self.0
            .iter()
            .flat_map(|(k, v)| k.iter().chain(iter::once(v)))
            .any(|r| r.has_side_effects())
    }
}

impl Table {
    pub(crate) fn without_shadowed_literal_fields(&self) -> Self {
        let mut fields = Vec::with_capacity(self.0.len());
        for (index, (key, value)) in self.0.iter().enumerate() {
            let shadowed = match key {
                Some(RValue::Literal(literal)) if !value.has_side_effects() => {
                    self.0[index + 1..].iter().any(|(later_key, _)| {
                        matches!(
                            later_key,
                            Some(RValue::Literal(later_literal)) if later_literal == literal
                        )
                    })
                }
                _ => false,
            };
            if !shadowed {
                fields.push((key.clone(), value.clone()));
            }
        }
        Self(fields)
    }
}

/*impl fmt::Display for Table {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{{{}}}",
            self.0
                .iter()
                .map(|(key, value)| match key {
                    Some(key) => format!("{} = {}", key, value),
                    None => value.to_string(),
                })
                .join(", ")
        )
    }
}*/

impl fmt::Display for Table {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Formatter {
            indentation_level: 0,
            indentation_mode: Default::default(),
            output: f,
        }
        .format_table(self)
    }
}

#[cfg(test)]
mod tests {
    use super::Table;
    use crate::{Literal, RValue};

    #[test]
    fn formatting_removes_shadowed_constant_template_fields() {
        let key: RValue = Literal::String(b"name".to_vec()).into();
        let table = Table(vec![
            (Some(key.clone()), Literal::Number(0.0).into()),
            (Some(key), Literal::Number(7.0).into()),
        ]);

        assert_eq!(table.to_string(), "{\n\tname = 7\n}");
    }
}
