use std::iter;
use std::{
    borrow::Cow,
    fmt::{self},
};

use itertools::Itertools;

use crate::{
    Assign, Binary, BinaryOperation, Block, Call, Class, Closure, Conditional, GenericFor, If,
    Index, LValue, Literal, MethodCall, NumericFor, RValue, Repeat, Return, Select, Statement,
    Table, Unary, While,
};

#[derive(Clone, Copy)]
pub enum IndentationMode {
    Spaces(u8),
    Tab,
}

impl IndentationMode {
    pub fn display(&self, out: &mut impl fmt::Write, indentation_level: usize) -> fmt::Result {
        let string = match self {
            Self::Spaces(spaces) => Cow::Owned(" ".repeat(*spaces as usize)),
            Self::Tab => Cow::Borrowed("\u{09}"),
        };
        for _ in 0..indentation_level {
            out.write_str(&string)?;
        }
        Ok(())
    }
}

impl fmt::Display for IndentationMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.display(f, 1)
    }
}

impl Default for IndentationMode {
    fn default() -> Self {
        Self::Tab
    }
}

/// Line width at which an argument list is wrapped to one argument per
/// line instead of staying on a single line. Folding (see `fold.rs`)
/// merges statements together, which can push an argument list that was
/// short on its own past a readable width; this is the only place that
/// budget is enforced.
const COLUMN_BUDGET: usize = 120;

pub(crate) fn format_arg_list(list: &[RValue]) -> String {
    let mut s = String::new();
    for (index, rvalue) in list.iter().enumerate() {
        if index + 1 == list.len() {
            if matches!(rvalue, RValue::Select(_)) {
                s += &format!("({})", rvalue);
            } else {
                s += &rvalue.to_string();
            }
        } else {
            s += &format!("{}, ", rvalue);
        }
    }
    s
}

pub struct Formatter<'a, W: fmt::Write> {
    pub(crate) indentation_level: usize,
    pub(crate) indentation_mode: IndentationMode,
    pub(crate) output: &'a mut W,
}

impl<'a, W: fmt::Write> Formatter<'a, W> {
    pub fn format(
        main: &Block,
        output: &'a mut W,
        indentation_mode: IndentationMode,
    ) -> fmt::Result {
        let mut formatter = Self {
            indentation_level: 0,
            indentation_mode,
            output,
        };
        formatter.format_block_no_indent(main)
    }

    fn indent(&mut self) -> fmt::Result {
        self.indentation_mode
            .display(&mut self.output, self.indentation_level)
    }

    // (function() end)()
    // (function() end)[1]
    fn should_wrap_left_rvalue(value: &RValue) -> bool {
        !matches!(
            value,
            RValue::Local(_)
                | RValue::Global(_)
                | RValue::Index(_)
                | RValue::Select(Select::Call(_) | Select::MethodCall(_))
        )
    }

    fn starts_with_parenthesis(value: &RValue) -> bool {
        if Self::should_wrap_left_rvalue(value) {
            return true;
        }
        match value {
            RValue::Index(index) => Self::starts_with_parenthesis(&index.left),
            _ => false,
        }
    }

    fn format_block(&mut self, block: &Block) -> fmt::Result {
        self.indentation_level += 1;
        self.format_block_no_indent(block)?;
        self.indentation_level -= 1;
        Ok(())
    }

    fn value_renders_multiline(value: &RValue) -> bool {
        match value {
            RValue::Closure(closure) => !closure.function.lock().body.is_empty(),
            RValue::Table(table) => Self::table_renders_multiline(table),
            _ => false,
        }
    }

    pub(crate) fn statement_renders_multiline(statement: &Statement) -> bool {
        match statement {
            Statement::If(_)
            | Statement::While(_)
            | Statement::Repeat(_)
            | Statement::NumericFor(_)
            | Statement::GenericFor(_) => true,
            Statement::Assign(assign) => {
                assign.right.iter().any(Self::value_renders_multiline)
            }
            _ => false,
        }
    }

    fn is_declaration(statement: &Statement) -> bool {
        matches!(statement, Statement::Assign(assign) if assign.prefix)
    }

    /// Whether a blank line belongs between two adjacent statements.
    ///
    /// `declaration_run` is the number of declarations immediately preceding
    /// `next`, so a block of locals can be separated from the work that uses
    /// them without splitting the block itself.
    ///
    /// `preceding_count` is `next`'s index within its block, i.e. how many
    /// statements come before it. A lone simple statement followed by
    /// `return` (`preceding_count == 1`) reads as one thought and stays
    /// tight; that carve-out only applies here because it is reached solely
    /// when `previous` isn't itself multi-line — the multi-line check above
    /// already returned `true` otherwise.
    fn needs_blank_between(
        previous: &Statement,
        next: &Statement,
        declaration_run: usize,
        preceding_count: usize,
    ) -> bool {
        if matches!(previous, Statement::Comment(_) | Statement::Empty(_))
            || matches!(next, Statement::Comment(_) | Statement::Empty(_))
        {
            return false;
        }
        if Self::statement_renders_multiline(previous)
            || Self::statement_renders_multiline(next)
        {
            return true;
        }
        if matches!(next, Statement::Return(_)) {
            return preceding_count != 1;
        }
        declaration_run >= 2 && !Self::is_declaration(next)
    }

    fn format_block_no_indent(&mut self, block: &Block) -> fmt::Result {
        let mut declaration_run = 0usize;
        for (i, statement) in block.iter().enumerate() {
            if i != 0 {
                writeln!(self.output)?;
                if Self::needs_blank_between(
                    &block[i - 1],
                    statement,
                    declaration_run,
                    i,
                ) {
                    writeln!(self.output)?;
                }
            }
            declaration_run = if Self::is_declaration(statement) {
                declaration_run + 1
            } else {
                0
            };
            self.format_statement(statement)?;
            if let Some(next_statement) =
                block.iter().skip(i + 1).find(|s| s.as_comment().is_none())
            {
                fn is_ambiguous(r: &RValue) -> bool {
                    match r {
                        RValue::Local(_)
                        | RValue::Global(_)
                        | RValue::Index(_)
                        | RValue::Call(_)
                        | RValue::MethodCall(_)
                        | RValue::Select(Select::Call(_) | Select::MethodCall(_)) => true,
                        RValue::Binary(binary) => is_ambiguous(&binary.right),
                        RValue::Conditional(conditional) => is_ambiguous(&conditional.else_value),
                        _ => false,
                    }
                }

                let disambiguate = match statement {
                    Statement::Call(_) | Statement::MethodCall(_) => true,
                    Statement::Repeat(repeat) => is_ambiguous(&repeat.condition),
                    Statement::Assign(Assign { right: list, .. })
                    | Statement::Return(Return { values: list }) => {
                        if let Some(last) = list.last() {
                            is_ambiguous(last)
                        } else {
                            false
                        }
                    }
                    Statement::Goto(_) | Statement::Continue(_) | Statement::Break(_) => true,
                    _ => false,
                };
                let disambiguate = disambiguate
                    && match next_statement {
                        Statement::Assign(Assign {
                            left,
                            prefix: false,
                            ..
                        }) => {
                            if let Some(index) = left[0].as_index() {
                                Self::starts_with_parenthesis(&index.left)
                            } else {
                                false
                            }
                        }
                        Statement::Call(Call { value, .. })
                        | Statement::MethodCall(MethodCall { value, .. }) => {
                            Self::starts_with_parenthesis(value)
                        }
                        Statement::Comment(_) => unimplemented!(),
                        _ => false,
                    };
                if disambiguate {
                    write!(self.output, ";")?;
                }
            }
        }
        Ok(())
    }

    fn format_lvalue(&mut self, lvalue: &LValue) -> fmt::Result {
        match lvalue {
            LValue::Index(index) => self.format_index(index),
            _ => write!(self.output, "{}", lvalue),
        }
    }

    fn are_table_keys_sequential(table: &Table) -> bool {
        let mut implicit_key = 0usize;
        table.0.iter().enumerate().all(|(index, (key, _))| {
            let expected = index + 1;
            match key {
                None => {
                    implicit_key += 1;
                    implicit_key == expected
                }
                Some(RValue::Literal(Literal::Number(key))) => {
                    key.is_finite() && *key == expected as f64
                }
                Some(RValue::Literal(Literal::Integer(key))) => {
                    usize::try_from(*key).is_ok_and(|key| key == expected)
                }
                _ => false,
            }
        })
    }

    fn contains_table(table: &Table) -> bool {
        table.0.iter().any(|(_, v)| matches!(v, RValue::Table(_x)))
    }

    fn stable_compound_index_component(value: &RValue) -> bool {
        matches!(value, RValue::Local(_) | RValue::Literal(_))
            || matches!(
                value,
                RValue::Global(global)
                    if global.origin() == crate::GlobalOrigin::CompilerImport
            )
    }

    fn compound_assignment(assign: &Assign) -> Option<(&LValue, BinaryOperation, &RValue)> {
        if assign.prefix || assign.parallel || assign.left.len() != 1 || assign.right.len() != 1 {
            return None;
        }
        let binary = assign.right[0].as_binary()?;
        if !matches!(
            binary.operation,
            BinaryOperation::Add
                | BinaryOperation::Sub
                | BinaryOperation::Mul
                | BinaryOperation::Div
                | BinaryOperation::IDiv
                | BinaryOperation::Mod
                | BinaryOperation::Pow
                | BinaryOperation::Concat
        ) {
            return None;
        }

        let target = &assign.left[0];
        let same_target = match (target, binary.left.as_ref()) {
            (LValue::Local(target), RValue::Local(read)) => target == read,
            (LValue::Index(target), RValue::Index(read)) => {
                target == read
                    && Self::stable_compound_index_component(&target.left)
                    && Self::stable_compound_index_component(&target.right)
            }
            _ => false,
        };
        same_target.then_some((target, binary.operation, binary.right.as_ref()))
    }

    /// Whether an already-compacted table (see
    /// `Table::without_shadowed_literal_fields`) will have its fields placed
    /// on their own lines.
    ///
    /// The blank-line predicate and the table writer must agree, so both
    /// funnel through this one definition rather than each deciding for
    /// themselves. Callers that haven't compacted their table yet should use
    /// `table_renders_multiline` instead.
    fn compacted_table_renders_multiline(compacted: &Table) -> bool {
        let sequential_keys = Self::are_table_keys_sequential(compacted);
        !compacted.0.is_empty() && (!sequential_keys || compacted.0.len() > 3)
            || Self::contains_table(compacted)
    }

    /// Whether `format_table` will place this table's fields on their own
    /// lines.
    pub(crate) fn table_renders_multiline(table: &Table) -> bool {
        Self::compacted_table_renders_multiline(&table.without_shadowed_literal_fields())
    }

    pub(crate) fn format_table(&mut self, table: &Table) -> fmt::Result {
        let compacted = table.without_shadowed_literal_fields();
        let table = &compacted;
        let sequential_keys = Self::are_table_keys_sequential(table);
        let should_space = !table.0.is_empty();
        let should_format = Self::compacted_table_renders_multiline(table);
        write!(self.output, "{{")?;
        if should_format {
            writeln!(self.output)?;
        } else if should_space {
            write!(self.output, " ")?;
        }
        self.indentation_level += 1;
        for (index, (key, value)) in table.0.iter().enumerate() {
            if should_format {
                self.indent()?;
            }
            let is_last = index + 1 == table.0.len();
            if is_last && (key.is_none() || sequential_keys) {
                let wrap = matches!(value, RValue::Select(_));
                if wrap {
                    write!(self.output, "(")?;
                }
                self.format_rvalue(value)?;
                if wrap {
                    write!(self.output, ")")?;
                }
            } else {
                if !sequential_keys {
                    if let Some(key) = key {
                        if let RValue::Literal(Literal::String(field)) = key
                            && Self::is_valid_name_in(field, crate::IdentifierContext::TableField)
                        {
                            write!(self.output, "{} = ", std::str::from_utf8(field).unwrap())?;
                        } else {
                            write!(self.output, "[")?;
                            self.format_rvalue(key)?;
                            write!(self.output, "] = ")?;
                        }
                    }
                }
                self.format_rvalue(value)?;
                if !is_last {
                    write!(self.output, ",")?;
                    write!(self.output, "{}", if should_format { "\n" } else { " " })?;
                }
            }
        }
        self.indentation_level -= 1;
        if should_format {
            writeln!(self.output)?;
            self.indent()?;
        } else if should_space {
            write!(self.output, " ")?;
        }
        write!(self.output, "}}")
    }

    pub(crate) fn format_unary(&mut self, unary: &Unary) -> fmt::Result {
        write!(self.output, "{}", unary.operation)?;
        let wrap = unary.group();
        if wrap {
            write!(self.output, "(")?;
        }
        self.format_rvalue(&unary.value)?;
        if wrap {
            write!(self.output, ")")?;
        }
        Ok(())
    }

    pub(crate) fn format_binary(&mut self, binary: &Binary) -> fmt::Result {
        let parentheses = |f: &mut Self, wrap: bool, rvalue: &RValue| -> fmt::Result {
            if wrap {
                write!(f.output, "(")?;
            }
            f.format_rvalue(rvalue)?;
            if wrap {
                write!(f.output, ")")?;
            }
            Ok(())
        };

        parentheses(self, binary.left_group(), &binary.left)?;
        write!(self.output, " {} ", binary.operation)?;
        parentheses(self, binary.right_group(), &binary.right)
    }

    fn format_closure_parameters_from(
        &mut self,
        closure: &Closure,
        skip_parameters: usize,
    ) -> fmt::Result {
        let function = closure.function.lock();
        write!(
            self.output,
            "{}",
            if function.is_variadic {
                function
                    .parameters
                    .iter()
                    .skip(skip_parameters)
                    .map(|x| x.to_string())
                    .chain(std::iter::once("...".into()))
                    .join(", ")
            } else {
                function.parameters.iter().skip(skip_parameters).join(", ")
            }
        )
    }

    fn format_closure_parameters(&mut self, closure: &Closure) -> fmt::Result {
        self.format_closure_parameters_from(closure, 0)
    }

    fn format_closure_body(&mut self, closure: &Closure) -> fmt::Result {
        let function = closure.function.lock();
        if !function.body.is_empty() {
            writeln!(self.output)?;
            self.indentation_level += 1;
            // if closure.name.is_some() {
            //     self.indent()?;
            //     writeln!(self.output, "-- function name: {}", closure.name.as_ref().unwrap())?;
            // }
            // if closure.line_defined.is_some() {
            //     self.indent()?;
            //     writeln!(self.output, "-- line defined: {}", closure.line_defined.as_ref().unwrap())?;
            // }
            self.indentation_level -= 1;

            self.format_block(&function.body)?;
            writeln!(self.output)?;
            self.indent()
        } else {
            write!(self.output, " ")
        }
    }

    pub(crate) fn format_closure(&mut self, closure: &Closure) -> fmt::Result {
        write!(self.output, "function(")?;
        self.format_closure_parameters(closure)?;
        write!(self.output, ")")?;
        self.format_closure_body(closure)?;
        write!(self.output, "end")
    }

    fn format_named_function(&mut self, name: &LValue, closure: &Closure) -> fmt::Result {
        let is_method = closure.function.lock().is_method;
        if is_method
            && let LValue::Index(index) = name
            && let RValue::Literal(Literal::String(method)) = index.right.as_ref()
            && Self::is_valid_name_in(method, crate::IdentifierContext::MethodName)
        {
            write!(self.output, "function ")?;
            self.format_rvalue(&index.left)?;
            write!(
                self.output,
                ":{}(",
                std::str::from_utf8(method).expect("valid method names are UTF-8")
            )?;
            self.format_closure_parameters_from(closure, 1)?;
            write!(self.output, ")")?;
            self.format_closure_body(closure)?;
            return write!(self.output, "end");
        }

        write!(self.output, "function {}(", name)?;
        self.format_closure_parameters(closure)?;
        write!(self.output, ")")?;
        self.format_closure_body(closure)?;
        write!(self.output, "end")
    }

    pub(crate) fn format_class(&mut self, class: &Class) -> fmt::Result {
        writeln!(self.output, "class {}", class.target)?;
        self.indentation_level += 1;

        for property in &class.properties {
            self.indent()?;
            writeln!(self.output, "public {property}")?;
        }

        for (name, value) in &class.methods {
            self.indent()?;
            if let RValue::Closure(closure) = value {
                write!(self.output, "function {name}(")?;
                self.format_closure_parameters(closure)?;
                write!(self.output, ")")?;
                self.format_closure_body(closure)?;
                writeln!(self.output, "end")?;
            } else {
                writeln!(self.output, "function {name}(...)")?;
                self.indentation_level += 1;
                self.indent()?;
                write!(self.output, "return ")?;
                self.format_rvalue(value)?;
                writeln!(self.output, "(...)")?;
                self.indentation_level -= 1;
                self.indent()?;
                writeln!(self.output, "end")?;
            }
        }

        self.indentation_level -= 1;
        self.indent()?;
        write!(self.output, "end")
    }

    fn format_rvalue(&mut self, rvalue: &RValue) -> fmt::Result {
        match rvalue {
            RValue::Select(Select::Call(call)) | RValue::Call(call) => self.format_call(call),
            RValue::Select(Select::MethodCall(method_call)) | RValue::MethodCall(method_call) => {
                self.format_method_call(method_call)
            }
            RValue::Table(table) => self.format_table(table),
            RValue::Index(index) => self.format_index(index),
            RValue::Unary(unary) => self.format_unary(unary),
            RValue::Binary(binary) => self.format_binary(binary),
            RValue::Conditional(conditional) => self.format_conditional(conditional),
            RValue::Closure(closure) => self.format_closure(closure),
            RValue::Literal(Literal::Number(n)) if n.is_infinite() => {
                // TODO: only insert parentheses when necessary
                write!(self.output, "(")?;
                self.format_binary(&Binary::new(
                    Literal::Number(if n.is_sign_positive() { 1.0 } else { -1.0 }).into(),
                    Literal::Number(0.0).into(),
                    BinaryOperation::Div,
                ))?;
                write!(self.output, ")")
            }
            RValue::Literal(Literal::Number(n)) if n.is_nan() => {
                // TODO: check that nan is appropriate for platform
                // assert_eq!(n.to_bits(), 0x7ff8000000000000);
                // TODO: only insert parentheses when necessary
                write!(self.output, "(")?;
                self.format_binary(&Binary::new(
                    Literal::Number(0.0).into(),
                    Literal::Number(0.0).into(),
                    BinaryOperation::Div,
                ))?;
                write!(self.output, ")")
            }
            _ => write!(self.output, "{}", rvalue),
        }
    }

    pub(crate) fn format_conditional(&mut self, conditional: &Conditional) -> fmt::Result {
        write!(self.output, "if ")?;
        self.format_rvalue(&conditional.condition)?;
        write!(self.output, " then ")?;
        self.format_rvalue(&conditional.then_value)?;
        if let RValue::Conditional(else_if) = conditional.else_value.as_ref() {
            write!(self.output, " else")?;
            self.format_conditional(else_if)
        } else {
            write!(self.output, " else ")?;
            self.format_rvalue(&conditional.else_value)
        }
    }

    /// Number of columns `indent()` currently emits.
    fn indentation_width(&self) -> usize {
        let per_level = match self.indentation_mode {
            IndentationMode::Spaces(spaces) => spaces as usize,
            IndentationMode::Tab => 1,
        };
        per_level * self.indentation_level
    }

    fn format_arg_list(&mut self, list: &[RValue]) -> fmt::Result {
        // A single argument has no sibling to move off its line, so
        // wrapping it can only add an indentation level without removing
        // anything — never a net improvement under this column-only model
        // (it has no notion of the call prefix sharing that line). Likewise
        // an argument that already renders multiline (a table or closure
        // with a body) contributes only its own short opening token to the
        // shared line, so if the list is over budget with one of those
        // present, the excess belongs to that argument's own content or to
        // the call's prefix, not to the argument list — wrapping the list
        // wouldn't fix it. Only multi-argument lists of otherwise
        // single-line values are candidates.
        //
        // A cheap sum of each argument's own display length (no
        // separators, no wrap parentheses) is enough to rule out the
        // common case of a short list without paying for a scratch
        // render.
        if list.len() > 1 && !list.iter().any(Self::value_renders_multiline) {
            let cheap_estimate = self.indentation_width()
                + list.iter().map(|rvalue| rvalue.to_string().len()).sum::<usize>()
                + (list.len() - 1) * ", ".len();
            if cheap_estimate > COLUMN_BUDGET {
                let mut scratch = String::new();
                Formatter {
                    indentation_level: self.indentation_level,
                    indentation_mode: self.indentation_mode,
                    output: &mut scratch,
                }
                .format_arg_list_inline(list)?;
                if self.indentation_width() + scratch.len() > COLUMN_BUDGET {
                    return self.format_arg_list_wrapped(list);
                }
                return write!(self.output, "{scratch}");
            }
        }
        self.format_arg_list_inline(list)
    }

    fn format_arg_list_inline(&mut self, list: &[RValue]) -> fmt::Result {
        for (index, rvalue) in list.iter().enumerate() {
            if index + 1 == list.len() {
                let wrap = matches!(rvalue, RValue::Select(_));
                if wrap {
                    write!(self.output, "(")?;
                }
                self.format_rvalue(rvalue)?;
                if wrap {
                    write!(self.output, ")")?;
                }
            } else {
                self.format_rvalue(rvalue)?;
                write!(self.output, ", ")?;
            }
        }
        Ok(())
    }

    /// One argument per line, at one indentation level deeper than the
    /// call itself. Leaves the cursor positioned right after `indent()` at
    /// the call's own level, ready for the caller to close with `)`.
    fn format_arg_list_wrapped(&mut self, list: &[RValue]) -> fmt::Result {
        writeln!(self.output)?;
        self.indentation_level += 1;
        for (index, rvalue) in list.iter().enumerate() {
            self.indent()?;
            let is_last = index + 1 == list.len();
            let wrap = is_last && matches!(rvalue, RValue::Select(_));
            if wrap {
                write!(self.output, "(")?;
            }
            self.format_rvalue(rvalue)?;
            if wrap {
                write!(self.output, ")")?;
            }
            writeln!(self.output, "{}", if is_last { "" } else { "," })?;
        }
        self.indentation_level -= 1;
        self.indent()
    }

    pub(crate) fn is_valid_name_in(name: &[u8], context: crate::IdentifierContext) -> bool {
        crate::is_valid_identifier_in(name, context)
    }

    // TODO: PERF: Cow like from_utf8_lossy
    pub(crate) fn escape_string(string: &[u8]) -> Cow<str> {
        let mut owned: Option<String> = None;
        let mut iter = string.iter().enumerate().peekable();
        while let Some((i, &c)) = iter.next() {
            if c == b' ' || (c.is_ascii_graphic() && c != b'\\' && c != b'\'' && c != b'\"') {
                if let Some(owned) = &mut owned {
                    owned.push(c as char);
                }
            } else {
                if owned.is_none() {
                    // TODO: PERF: unchecked?
                    owned = Some(std::str::from_utf8(&string[..i]).unwrap().to_string());
                    // TODO: do we want to be multiplying by 2 here?
                    // TODO: PERF: String::with_capacity + push_str to avoid an allocation
                    owned.as_mut().unwrap().reserve((string.len() - i) * 2);
                }
                let owned = owned.as_mut().unwrap();
                match c {
                    b'\n' => owned.push_str(r"\n"),
                    b'\r' => owned.push_str(r"\r"),
                    b'\t' => owned.push_str(r"\t"),
                    b'\"' => owned.push_str(r#"\""#),
                    b'\'' => owned.push_str(r"\'"),
                    b'\\' => owned.push_str(r"\\"),
                    12 => owned.push_str(r"\f"),
                    _ => {
                        let mut buffer = itoa::Buffer::new();
                        let printed = buffer.format(c);
                        owned.push('\\');
                        if printed.len() != 3
                            && let Some((_, next)) = iter.peek()
                            && next.is_ascii_digit()
                        {
                            owned.extend(iter::repeat('0').take(3 - printed.len()));
                        }
                        owned.push_str(printed);
                    }
                };
            }
        }
        if let Some(owned) = owned {
            owned.into()
        } else {
            // TODO: PERF: unchecked?
            std::str::from_utf8(string).unwrap().into()
        }
    }

    pub(crate) fn format_index(&mut self, index: &Index) -> fmt::Result {
        let wrap = Self::should_wrap_left_rvalue(&index.left);
        if wrap {
            write!(self.output, "(")?;
        }
        self.format_rvalue(&index.left)?;
        if wrap {
            write!(self.output, ")")?;
        }

        match index.right.as_ref() {
            RValue::Literal(super::Literal::String(field))
                if Self::is_valid_name_in(field, crate::IdentifierContext::MemberName) =>
            {
                write!(self.output, ".{}", std::str::from_utf8(field).unwrap())
            }
            _ => {
                write!(self.output, "[")?;
                self.format_rvalue(&index.right)?;
                write!(self.output, "]")
            }
        }
    }

    pub(crate) fn format_call(&mut self, call: &Call) -> fmt::Result {
        let wrap = Self::should_wrap_left_rvalue(&call.value);
        if wrap {
            write!(self.output, "(")?;
        }
        self.format_rvalue(&call.value)?;
        if wrap {
            write!(self.output, ")")?;
        }

        write!(self.output, "(")?;
        self.format_arg_list(&call.arguments)?;
        write!(self.output, ")")
    }

    pub(crate) fn format_method_call(&mut self, method_call: &MethodCall) -> fmt::Result {
        let wrap = Self::should_wrap_left_rvalue(&method_call.value);
        if wrap {
            write!(self.output, "(")?;
        }
        self.format_rvalue(&method_call.value)?;
        if wrap {
            write!(self.output, ")")?;
        }

        write!(self.output, ":{}", method_call.method)?;

        write!(self.output, "(")?;
        self.format_arg_list(&method_call.arguments)?;
        write!(self.output, ")")
    }

    pub(crate) fn format_if(&mut self, r#if: &If) -> fmt::Result {
        write!(self.output, "if ")?;

        self.format_rvalue(&r#if.condition)?;

        writeln!(self.output, " then")?;

        let then_block = r#if.then_block.lock();
        if !then_block.is_empty() {
            self.format_block(&then_block)?;
            writeln!(self.output)?;
        }

        let else_block = r#if.else_block.lock();
        if !else_block.is_empty() {
            self.indent()?;
            if let Some(else_if) = else_block.iter().exactly_one().ok().and_then(|s| s.as_if()) {
                write!(self.output, "else")?;
                return self.format_if(else_if);
            }
            writeln!(self.output, "else")?;
            self.format_block(&else_block)?;
            writeln!(self.output)?;
        }

        self.indent()?;
        write!(self.output, "end")
    }

    pub(crate) fn format_assign(&mut self, assign: &Assign) -> fmt::Result {
        if assign.prefix {
            write!(self.output, "local ")?;
        }

        if assign.left.len() == 1
            && assign.right.len() == 1
            && let RValue::Closure(closure) = &assign.right[0]
        {
            let left = &assign.left[0];
            let recursive_local = assign.prefix
                && left.as_local().is_some_and(|local| {
                    closure.upvalues.iter().any(|upvalue| {
                        matches!(
                            upvalue,
                            crate::Upvalue::Copy(captured) | crate::Upvalue::Ref(captured)
                                if captured == local
                        )
                    })
                });
            if (closure.function.lock().name.is_some() || recursive_local)
                && (assign.prefix || left.as_global().is_some() || {
                    if let LValue::Index(index) = left {
                        let mut index = index;
                        let mut valid = true;
                        loop {
                            if let box RValue::Literal(Literal::String(key)) = &index.right
                                && Self::is_valid_name_in(key, crate::IdentifierContext::MemberName)
                            {
                                match index.left {
                                    box RValue::Index(ref i) => {
                                        index = i;
                                        continue;
                                    }
                                    box RValue::Global(_) | box RValue::Local(_) => {}
                                    _ => valid = false,
                                }
                            } else {
                                valid = false;
                            }
                            break;
                        }
                        valid
                    } else {
                        false
                    }
                })
            {
                return self.format_named_function(left, closure);
            }
        }

        if let Some((target, operation, value)) = Self::compound_assignment(assign) {
            self.format_lvalue(target)?;
            write!(self.output, " {}= ", operation)?;
            return self.format_rvalue(value);
        }

        for (i, lvalue) in assign.left.iter().enumerate() {
            if i != 0 {
                write!(self.output, ", ")?;
            }
            self.format_lvalue(lvalue)?;
        }

        if !assign.right.is_empty() {
            write!(self.output, " = ")?;
        } else {
            assert!(assign.prefix);
        }

        // TODO: REFACTOR: move to format_rvalue_list function
        for (i, rvalue) in assign.right.iter().enumerate() {
            if i != 0 {
                write!(self.output, ", ")?;
            }
            let remaining_targets = assign.left.len().saturating_sub(i);
            let wrap = i + 1 == assign.right.len()
                && remaining_targets > 1
                && matches!(rvalue, RValue::Select(_));
            if wrap {
                write!(self.output, "(")?;
            }
            self.format_rvalue(rvalue)?;
            if wrap {
                write!(self.output, ")")?;
            }
        }

        if assign.parallel {
            write!(self.output, " -- parallel")?;
        }

        Ok(())
    }

    pub(crate) fn format_while(&mut self, r#while: &While) -> fmt::Result {
        write!(self.output, "while ")?;

        self.format_rvalue(&r#while.condition)?;

        writeln!(self.output, " do")?;

        self.format_block(&r#while.block.lock())?;
        writeln!(self.output)?;
        self.indent()?;
        write!(self.output, "end")
    }

    pub(crate) fn format_repeat(&mut self, r#repeat: &Repeat) -> fmt::Result {
        writeln!(self.output, "repeat")?;
        self.format_block(&repeat.block.lock())?;
        writeln!(self.output)?;
        self.indent()?;

        write!(self.output, "until ")?;

        self.format_rvalue(&repeat.condition)
    }

    pub(crate) fn format_numeric_for(&mut self, numeric_for: &NumericFor) -> fmt::Result {
        write!(self.output, "for {} = ", numeric_for.counter)?;
        self.format_rvalue(&numeric_for.initial)?;
        write!(self.output, ", ")?;
        self.format_rvalue(&numeric_for.limit)?;
        let skip_step = if let RValue::Literal(Literal::Number(n)) = numeric_for.step {
            n == 1.0
        } else {
            false
        };
        if !skip_step {
            write!(self.output, ", ")?;
            self.format_rvalue(&numeric_for.step)?;
        }
        writeln!(self.output, " do")?;
        self.format_block(&numeric_for.block.lock())?;
        writeln!(self.output)?;
        self.indent()?;
        write!(self.output, "end")
    }

    pub(crate) fn format_generic_for(&mut self, generic_for: &GenericFor) -> fmt::Result {
        write!(
            self.output,
            "for {} in ",
            generic_for.res_locals.iter().join(", ")
        )?;
        for (i, rvalue) in generic_for
            .right
            .iter()
            .enumerate()
            .rev()
            .skip_while(|(i, v)| *i != 0 && matches!(v, RValue::Literal(Literal::Nil)))
            .map(|(_, x)| x)
            .collect_vec()
            .iter()
            .rev()
            .enumerate()
        {
            if i != 0 {
                write!(self.output, ", ")?;
            }
            self.format_rvalue(rvalue)?;
        }
        writeln!(self.output, " do")?;
        self.format_block(&generic_for.block.lock())?;
        writeln!(self.output)?;
        self.indent()?;
        write!(self.output, "end")
    }

    pub(crate) fn format_return(&mut self, r#return: &Return) -> fmt::Result {
        write!(self.output, "return")?;
        for (i, rvalue) in r#return.values.iter().enumerate() {
            if i == 0 {
                write!(self.output, " ")?;
            } else {
                write!(self.output, ", ")?;
            }
            let wrap = i + 1 == r#return.values.len() && matches!(rvalue, RValue::Select(_));
            if wrap {
                write!(self.output, "(")?;
            }
            self.format_rvalue(rvalue)?;
            if wrap {
                write!(self.output, ")")?;
            }
        }

        Ok(())
    }

    fn format_statement(&mut self, statement: &Statement) -> fmt::Result {
        self.indent()?;

        match statement {
            Statement::Assign(assign) => self.format_assign(assign),
            Statement::Class(class) => self.format_class(class),
            Statement::If(r#if) => self.format_if(r#if),
            Statement::While(r#while) => self.format_while(r#while),
            Statement::Repeat(repeat) => self.format_repeat(repeat),
            Statement::NumericFor(numeric_for) => self.format_numeric_for(numeric_for),
            Statement::GenericFor(generic_for) => self.format_generic_for(generic_for),
            Statement::Call(call) => self.format_call(call),
            Statement::MethodCall(method_call) => self.format_method_call(method_call),
            Statement::Return(r#return) => self.format_return(r#return),
            _ => write!(self.output, "{}", statement),
        }
    }
}

#[cfg(test)]
mod tests {
    use by_address::ByAddress;
    use parking_lot::Mutex;
    use triomphe::Arc;

    use crate::{
        Assign, Binary, BinaryOperation, Block, Call, Closure, Comment, Conditional, Empty,
        Function, Global, Index, LValue, Literal, Local, MethodCall, NumericFor, RValue, RcLocal,
        ResultDemand, Return, Select, Table, Upvalue, VarArg,
    };

    fn local(name: &str) -> RcLocal {
        RcLocal::new(Local::new(Some(name.to_owned())))
    }

    #[test]
    fn separates_call_from_following_parenthesized_table_call() {
        let first = Call::new(Global::from("first").into(), Vec::new());
        let callable = Table(vec![(None, Global::from("callback").into())]);
        let indexed = Index::new(callable.into(), Literal::Number(1.0).into());
        let second = Call::new(indexed.into(), Vec::new());
        let block = Block(vec![first.into(), second.into()]);

        assert_eq!(block.to_string(), "first();\n({ callback })[1]()");
    }

    #[test]
    fn compound_formats_local_update() {
        let value = local("value");
        let increment = local("increment");
        let assign = Assign::new(
            vec![LValue::Local(value.clone())],
            vec![Binary::new(value.into(), increment.into(), BinaryOperation::Add).into()],
        );

        assert_eq!(assign.to_string(), "value += increment");
    }

    #[test]
    fn compound_formats_every_supported_operator() {
        let operations = [
            (BinaryOperation::Add, "+="),
            (BinaryOperation::Sub, "-="),
            (BinaryOperation::Mul, "*="),
            (BinaryOperation::Div, "/="),
            (BinaryOperation::IDiv, "//="),
            (BinaryOperation::Mod, "%="),
            (BinaryOperation::Pow, "^="),
            (BinaryOperation::Concat, "..="),
        ];

        for (operation, syntax) in operations {
            let value = local("value");
            let assign = Assign::new(
                vec![LValue::Local(value.clone())],
                vec![Binary::new(value.into(), local("next").into(), operation).into()],
            );
            assert_eq!(assign.to_string(), format!("value {syntax} next"));
        }
    }

    #[test]
    fn compound_formats_stable_index_update() {
        let object = local("object");
        let key = local("key");
        let increment = local("increment");
        let index = Index::new(object.into(), key.into());
        let assign = Assign::new(
            vec![LValue::Index(index.clone())],
            vec![Binary::new(RValue::Index(index), increment.into(), BinaryOperation::Add).into()],
        );

        assert_eq!(assign.to_string(), "object[key] += increment");
    }

    #[test]
    fn compound_keeps_non_equivalent_assignments_expanded() {
        let value = local("value");
        let other = local("other");
        let increment = local("increment");

        let different_left = Assign::new(
            vec![LValue::Local(value.clone())],
            vec![
                Binary::new(
                    other.clone().into(),
                    increment.clone().into(),
                    BinaryOperation::Add,
                )
                .into(),
            ],
        );
        let reversed = Assign::new(
            vec![LValue::Local(value.clone())],
            vec![Binary::new(other.into(), value.clone().into(), BinaryOperation::Add).into()],
        );
        let logical = Assign::new(
            vec![LValue::Local(value.clone())],
            vec![Binary::new(value.clone().into(), increment.into(), BinaryOperation::And).into()],
        );
        let multiple = Assign::new(
            vec![LValue::Local(value), LValue::Local(local("second"))],
            vec![RValue::Local(local("first")), RValue::Local(local("next"))],
        );

        assert_eq!(different_left.to_string(), "value = other + increment");
        assert_eq!(reversed.to_string(), "value = other + value");
        assert_eq!(logical.to_string(), "value = value and increment");
        assert_eq!(multiple.to_string(), "value, second = first, next");
    }

    #[test]
    fn compound_keeps_effectful_index_components_expanded() {
        let key = local("key");
        let increment = local("increment");
        let object = Call::new(Global::from("fetch").into(), Vec::new());
        let index = Index::new(object.into(), key.into());
        let assign = Assign::new(
            vec![LValue::Index(index.clone())],
            vec![Binary::new(RValue::Index(index), increment.into(), BinaryOperation::Add).into()],
        );

        assert_eq!(
            assign.to_string(),
            "(fetch())[key] = (fetch())[key] + increment"
        );

        let dynamic_global = Index::new(Global::from("object").into(), local("key").into());
        let global_assign = Assign::new(
            vec![LValue::Index(dynamic_global.clone())],
            vec![
                Binary::new(
                    dynamic_global.into(),
                    local("increment").into(),
                    BinaryOperation::Add,
                )
                .into(),
            ],
        );
        let calculated_key = Binary::new(
            local("left").into(),
            local("right").into(),
            BinaryOperation::Add,
        );
        let calculated = Index::new(local("object").into(), calculated_key.into());
        let calculated_assign = Assign::new(
            vec![LValue::Index(calculated.clone())],
            vec![
                Binary::new(
                    calculated.into(),
                    local("increment").into(),
                    BinaryOperation::Add,
                )
                .into(),
            ],
        );

        assert!(!global_assign.to_string().contains("+="));
        assert!(!calculated_assign.to_string().contains("+="));
    }

    #[test]
    fn selected_call_stays_single_result_in_assignment_tail() {
        let selected = Select::Call(Call::new(Global::from("produce").into(), Vec::new()));
        let assign = Assign::new(
            vec![
                LValue::Local(local("first")),
                LValue::Local(local("second")),
            ],
            vec![Literal::Number(1.0).into(), selected.into()],
        );

        assert_eq!(assign.to_string(), "first, second = 1, produce()");

        let selected = Select::Call(Call::new(Global::from("produce").into(), Vec::new()));
        let assign = Assign::new(
            vec![
                LValue::Local(local("first")),
                LValue::Local(local("second")),
            ],
            vec![selected.into()],
        );
        assert_eq!(assign.to_string(), "first, second = (produce())");
    }

    #[test]
    fn result_demand_matrix_preserves_fixed_and_open_list_semantics() {
        let selected_call = Select::Call(Call::new(Global::from("produce").into(), Vec::new()))
            .into_rvalue(ResultDemand::Exact(1));
        let exact_call = Select::Call(Call::new(Global::from("produce").into(), Vec::new()))
            .into_rvalue(ResultDemand::Exact(2));
        let call_assign = Assign::new(
            vec![
                LValue::Local(local("first")),
                LValue::Local(local("second")),
            ],
            vec![exact_call],
        );

        let exact_method = Select::MethodCall(MethodCall::new(
            local("object").into(),
            "produce".to_owned(),
            Vec::new(),
        ))
        .into_rvalue(ResultDemand::Exact(1));
        let method_return = Return::new(vec![exact_method]);
        let open_method = Select::MethodCall(MethodCall::new(
            local("object").into(),
            "produce".to_owned(),
            Vec::new(),
        ))
        .into_rvalue(ResultDemand::Open);
        let open_method_return = Return::new(vec![open_method]);

        let exact_vararg = Select::VarArg(VarArg).into_rvalue(ResultDemand::Exact(3));
        let vararg_assign = Assign::new(
            vec![
                LValue::Local(local("a")),
                LValue::Local(local("b")),
                LValue::Local(local("c")),
            ],
            vec![exact_vararg],
        );
        let open_vararg = Select::VarArg(VarArg).into_rvalue(ResultDemand::Open);
        let open_vararg_return = Return::new(vec![Literal::Number(1.0).into(), open_vararg]);

        let open_call = Select::Call(Call::new(Global::from("produce").into(), Vec::new()))
            .into_rvalue(ResultDemand::Open);
        let open_return = Return::new(vec![Literal::Number(1.0).into(), open_call]);

        assert_eq!(
            Return::new(vec![selected_call]).to_string(),
            "return (produce())"
        );
        assert_eq!(call_assign.to_string(), "first, second = produce()");
        assert_eq!(method_return.to_string(), "return (object:produce())");
        assert_eq!(open_method_return.to_string(), "return object:produce()");
        assert_eq!(vararg_assign.to_string(), "a, b, c = ...");
        assert_eq!(open_vararg_return.to_string(), "return 1, ...");
        assert_eq!(open_return.to_string(), "return 1, produce()");
        assert!(!ResultDemand::Exact(0).has_values());
    }

    #[test]
    fn conditional_tail_disambiguates_following_parenthesized_call() {
        let conditional = Conditional::new(
            local("condition").into(),
            local("selected").into(),
            local("fallback").into(),
        );
        let result = local("result");
        let next = Call::new(Table::default().into(), Vec::new());
        let block = Block(vec![
            Assign::new(vec![result.into()], vec![conditional.into()]).into(),
            next.into(),
        ]);

        assert_eq!(
            block.to_string(),
            "result = if condition then selected else fallback;\n({})()"
        );
    }

    #[test]
    fn sequential_table_field_keeps_selected_final_call_closed() {
        let selected = Select::Call(Call::new(Global::from("produce").into(), Vec::new()));
        let table = Table(vec![(Some(Literal::Number(1.0).into()), selected.into())]);

        assert_eq!(table.to_string(), "{ (produce()) }");
    }

    #[test]
    fn explicit_key_collision_after_positional_prefix_stays_explicit() {
        let table = Table(vec![
            (None, Literal::String(b"first".to_vec()).into()),
            (
                Some(Literal::Number(1.0).into()),
                Literal::String(b"replacement".to_vec()).into(),
            ),
        ]);

        assert_eq!(
            table.to_string(),
            "{\n\t\"first\",\n\t[1] = \"replacement\"\n}"
        );
    }

    #[test]
    fn fractional_numeric_table_key_stays_explicit() {
        let table = Table(vec![(
            Some(Literal::Number(1.5).into()),
            Literal::String(b"value".to_vec()).into(),
        )]);

        assert_eq!(table.to_string(), "{\n\t[1.5] = \"value\"\n}");
    }

    #[test]
    fn unnamed_recursive_local_closure_uses_scoped_function_declaration() {
        let recurse = local("recurse");
        let function = Arc::new(Mutex::new(Function::default()));
        let closure = Closure {
            function: ByAddress(function),
            upvalues: vec![Upvalue::Ref(recurse.clone())],
        };
        let mut assign = Assign::new(vec![LValue::Local(recurse)], vec![RValue::Closure(closure)]);
        assign.prefix = true;

        assert!(assign.to_string().starts_with("local function recurse()"));
    }

    #[test]
    fn blank_line_separates_a_multiline_statement_from_its_neighbours() {
        let counter = local("i");
        let block = Block(vec![
            Assign::new(vec![local("a").into()], vec![Literal::Number(1.0).into()]).into(),
            NumericFor::new(
                Literal::Number(1.0).into(),
                Literal::Number(2.0).into(),
                Literal::Number(1.0).into(),
                counter,
                Block(vec![
                    Assign::new(vec![local("b").into()], vec![Literal::Number(2.0).into()])
                        .into(),
                ]),
            )
            .into(),
            Assign::new(vec![local("c").into()], vec![Literal::Number(3.0).into()]).into(),
        ]);

        let formatted = block.to_string();

        assert_eq!(
            formatted,
            "a = 1\n\nfor i = 1, 2 do\n\tb = 2\nend\n\nc = 3"
        );
    }

    #[test]
    fn consecutive_single_line_statements_stay_tight() {
        let block = Block(vec![
            Assign::new(vec![local("a").into()], vec![Literal::Number(1.0).into()]).into(),
            Assign::new(vec![local("b").into()], vec![Literal::Number(2.0).into()]).into(),
            Assign::new(vec![local("c").into()], vec![Literal::Number(3.0).into()]).into(),
        ]);

        assert_eq!(block.to_string(), "a = 1\nb = 2\nc = 3");
    }

    #[test]
    fn two_statement_body_return_stays_tight() {
        // A single simple statement immediately followed by `return` reads
        // as one thought (e.g. `self.middleware[...] = p1` then
        // `return self`), so no blank line is inserted here even though
        // `return` follows other work in general.
        let block = Block(vec![
            Assign::new(vec![local("a").into()], vec![Literal::Number(1.0).into()]).into(),
            Return::new(vec![local("a").into()]).into(),
        ]);

        assert_eq!(block.to_string(), "a = 1\nreturn a");
    }

    #[test]
    fn three_statement_body_keeps_blank_before_return() {
        let block = Block(vec![
            Assign::new(vec![local("a").into()], vec![Literal::Number(1.0).into()]).into(),
            Assign::new(vec![local("b").into()], vec![Literal::Number(2.0).into()]).into(),
            Return::new(vec![local("a").into()]).into(),
        ]);

        assert_eq!(block.to_string(), "a = 1\nb = 2\n\nreturn a");
    }

    #[test]
    fn multiline_first_statement_then_return_keeps_blank() {
        let counter = local("i");
        let block = Block(vec![
            NumericFor::new(
                Literal::Number(1.0).into(),
                Literal::Number(2.0).into(),
                Literal::Number(1.0).into(),
                counter,
                Block(vec![
                    Assign::new(vec![local("b").into()], vec![Literal::Number(2.0).into()])
                        .into(),
                ]),
            )
            .into(),
            Return::new(vec![local("a").into()]).into(),
        ]);

        assert_eq!(
            block.to_string(),
            "for i = 1, 2 do\n\tb = 2\nend\n\nreturn a"
        );
    }

    #[test]
    fn a_lone_return_gains_no_leading_blank_line() {
        let block = Block(vec![Return::new(vec![local("a").into()]).into()]);

        assert_eq!(block.to_string(), "return a");
    }

    #[test]
    fn empty_closure_value_is_not_treated_as_multiline() {
        let closure = Closure {
            function: ByAddress(Arc::new(Mutex::new(Function::default()))),
            upvalues: Vec::new(),
        };
        let block = Block(vec![
            Assign::new(vec![local("f").into()], vec![closure.into()]).into(),
            Assign::new(vec![local("g").into()], vec![Literal::Number(1.0).into()]).into(),
        ]);

        assert_eq!(block.to_string(), "f = function() end\ng = 1");
    }

    #[test]
    fn comment_before_multiline_statement_suppresses_blank_line() {
        let counter = local("i");
        let block = Block(vec![
            Comment::new("note".to_owned()).into(),
            NumericFor::new(
                Literal::Number(1.0).into(),
                Literal::Number(2.0).into(),
                Literal::Number(1.0).into(),
                counter,
                Block(vec![
                    Assign::new(vec![local("b").into()], vec![Literal::Number(2.0).into()])
                        .into(),
                ]),
            )
            .into(),
        ]);

        assert_eq!(
            block.to_string(),
            "-- note\nfor i = 1, 2 do\n\tb = 2\nend"
        );
    }

    #[test]
    fn comment_after_multiline_statement_suppresses_blank_line() {
        let counter = local("i");
        let block = Block(vec![
            NumericFor::new(
                Literal::Number(1.0).into(),
                Literal::Number(2.0).into(),
                Literal::Number(1.0).into(),
                counter,
                Block(vec![
                    Assign::new(vec![local("b").into()], vec![Literal::Number(2.0).into()])
                        .into(),
                ]),
            )
            .into(),
            Comment::new("note".to_owned()).into(),
        ]);

        assert_eq!(
            block.to_string(),
            "for i = 1, 2 do\n\tb = 2\nend\n-- note"
        );
    }

    #[test]
    fn empty_statement_neighbour_suppresses_blank_line_on_both_sides() {
        // Without the comment/empty suppression, the transition out of the
        // multi-line `for` loop would gain a second blank line on top of the
        // one the `Empty` statement's own (content-free) line already reads
        // as.
        let counter = local("i");
        let block = Block(vec![
            NumericFor::new(
                Literal::Number(1.0).into(),
                Literal::Number(2.0).into(),
                Literal::Number(1.0).into(),
                counter,
                Block(vec![
                    Assign::new(vec![local("b").into()], vec![Literal::Number(2.0).into()])
                        .into(),
                ]),
            )
            .into(),
            Empty {}.into(),
            Assign::new(vec![local("a").into()], vec![Literal::Number(1.0).into()]).into(),
        ]);

        assert_eq!(
            block.to_string(),
            "for i = 1, 2 do\n\tb = 2\nend\n\na = 1"
        );
    }

    #[test]
    fn an_argument_list_past_the_column_budget_wraps_one_per_line() {
        let call = Call::new(
            local("someFunctionWithAVeryLongName").into(),
            (0..8)
                .map(|index| {
                    RValue::Local(local(&format!("argumentNumber{index}WithALongName")))
                })
                .collect(),
        );
        let block = Block(vec![call.into()]);

        let formatted = block.to_string();

        assert!(formatted.contains(",\n"));
        assert!(formatted.lines().all(|line| line.len() <= 120));
    }

    #[test]
    fn a_short_argument_list_stays_on_one_line() {
        let call = Call::new(local("f").into(), vec![local("a").into(), local("b").into()]);
        let block = Block(vec![call.into()]);

        assert_eq!(block.to_string(), "f(a, b)");
    }

    #[test]
    fn a_single_huge_string_argument_is_never_wrapped_and_stays_intact() {
        // A single argument can never gain a sibling to strip off its line
        // (`format_arg_list` requires `list.len() > 1` to even consider
        // wrapping), so this holds by construction. Pinned anyway: a Lua
        // string cannot be split without inserting `..`, which would change
        // the AST, so this call staying on one line — with the string
        // untouched — is the property that makes this whole feature
        // formatter-only. This must never regress silently.
        let huge = "x".repeat(300);
        let call = Call::new(
            local("f").into(),
            vec![RValue::Literal(Literal::String(huge.clone().into_bytes()))],
        );
        let block = Block(vec![call.into()]);

        let formatted = block.to_string();

        assert!(!formatted.contains(",\n"));
        assert!(formatted.contains(&huge));
    }
}
