//! A small, deliberately constrained expression evaluator.
//!
//! Expressions are tokenized and parsed into a private AST that can only
//! represent the allow-listed grammar.  There is no host-language `eval`, no
//! function calls, no attribute access to private names, and no way to reach
//! the filesystem, network, clock or environment.  The value domain is JSON,
//! which is also the canonical data model of the workflow descriptor.
//!
//! Semantics follow the Python evaluator this replaces, because existing
//! workflows depend on them: `/` is true division, `//` floors, comparison
//! chaining works, `and`/`or` return the deciding operand rather than a bool,
//! and mapping keys win over attribute lookups.

use serde_json::{Map, Number, Value};
use std::fmt;

/// Raised when an expression is invalid, unsafe, or cannot be evaluated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpressionError {
    pub message: String,
    pub source: Option<String>,
    pub lineno: Option<u32>,
    pub col_offset: Option<u32>,
}

impl ExpressionError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
            lineno: None,
            col_offset: None,
        }
    }

    fn at(message: impl Into<String>, source: &str, position: Position) -> Self {
        Self {
            message: message.into(),
            source: Some(source.to_string()),
            lineno: Some(position.line),
            col_offset: Some(position.column),
        }
    }
}

impl fmt::Display for ExpressionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)?;
        if let Some(line) = self.lineno {
            write!(formatter, " at line {line}")?;
            if let Some(column) = self.col_offset {
                write!(formatter, ", column {}", column + 1)?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for ExpressionError {}

type Result<T> = std::result::Result<T, ExpressionError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
struct Position {
    line: u32,
    column: u32,
}

/// `__name__`-style identifiers are rejected everywhere they can appear.
fn is_dunder(name: &str) -> bool {
    name.len() >= 4 && name.starts_with("__") && name.ends_with("__")
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Name(String),
    Int(i64),
    Float(f64),
    Str(String),
    Op(&'static str),
    End,
}

#[derive(Clone, Debug)]
struct Token {
    kind: Tok,
    position: Position,
}

/// Multi-character operators, longest first so `//` never lexes as two `/`.
const OPERATORS: &[&str] = &[
    "**", "//", "==", "!=", "<=", ">=", "(", ")", "[", "]", "{", "}", ",", ":", ".", "+", "-", "*",
    "/", "%", "<", ">",
];

fn tokenize(source: &str) -> Result<Vec<Token>> {
    let characters: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    let mut line = 1u32;
    let mut column = 0u32;

    while index < characters.len() {
        let character = characters[index];
        let position = Position { line, column };

        if character == '\n' {
            index += 1;
            line += 1;
            column = 0;
            continue;
        }
        if character.is_whitespace() {
            index += 1;
            column += 1;
            continue;
        }
        // A comment would let an expression hide its tail; reject outright.
        if character == '#' {
            return Err(ExpressionError::at(
                "comments are not allowed in expressions",
                source,
                position,
            ));
        }

        if character == '"' || character == '\'' {
            let (text, consumed) = read_string(&characters, index, source, position)?;
            tokens.push(Token {
                kind: Tok::Str(text),
                position,
            });
            column += consumed as u32;
            index += consumed;
            continue;
        }

        if character.is_ascii_digit()
            || (character == '.'
                && characters
                    .get(index + 1)
                    .is_some_and(char::is_ascii_digit))
        {
            let (token, consumed) = read_number(&characters, index, source, position)?;
            tokens.push(Token {
                kind: token,
                position,
            });
            column += consumed as u32;
            index += consumed;
            continue;
        }

        if character.is_alphabetic() || character == '_' {
            let start = index;
            while index < characters.len()
                && (characters[index].is_alphanumeric() || characters[index] == '_')
            {
                index += 1;
            }
            let word: String = characters[start..index].iter().collect();
            column += (index - start) as u32;
            tokens.push(Token {
                kind: Tok::Name(word),
                position,
            });
            continue;
        }

        let remainder: String = characters[index..].iter().collect();
        match OPERATORS.iter().find(|op| remainder.starts_with(**op)) {
            Some(operator) => {
                tokens.push(Token {
                    kind: Tok::Op(operator),
                    position,
                });
                index += operator.len();
                column += operator.len() as u32;
            }
            None => {
                return Err(ExpressionError::at(
                    format!("invalid expression syntax: unexpected character {character:?}"),
                    source,
                    position,
                ))
            }
        }
    }

    tokens.push(Token {
        kind: Tok::End,
        position: Position { line, column },
    });
    Ok(tokens)
}

fn read_string(
    characters: &[char],
    start: usize,
    source: &str,
    position: Position,
) -> Result<(String, usize)> {
    let quote = characters[start];
    let mut text = String::new();
    let mut index = start + 1;
    while index < characters.len() {
        let character = characters[index];
        if character == '\\' {
            let escaped = characters.get(index + 1).copied().ok_or_else(|| {
                ExpressionError::at("invalid expression syntax: dangling escape", source, position)
            })?;
            text.push(match escaped {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '0' => '\0',
                '\\' => '\\',
                '\'' => '\'',
                '"' => '"',
                other => other,
            });
            index += 2;
            continue;
        }
        if character == quote {
            return Ok((text, index + 1 - start));
        }
        text.push(character);
        index += 1;
    }
    Err(ExpressionError::at(
        "invalid expression syntax: unterminated string literal",
        source,
        position,
    ))
}

fn read_number(
    characters: &[char],
    start: usize,
    source: &str,
    position: Position,
) -> Result<(Tok, usize)> {
    let mut index = start;
    let mut is_float = false;
    while index < characters.len() {
        let character = characters[index];
        if character.is_ascii_digit() || character == '_' {
            index += 1;
        } else if character == '.' && !is_float {
            is_float = true;
            index += 1;
        } else if (character == 'e' || character == 'E')
            && index > start
            && characters
                .get(index + 1)
                .is_some_and(|next| next.is_ascii_digit() || *next == '+' || *next == '-')
        {
            is_float = true;
            index += 2;
        } else {
            break;
        }
    }
    let text: String = characters[start..index].iter().filter(|c| **c != '_').collect();
    let token = if is_float {
        Tok::Float(text.parse::<f64>().map_err(|_| {
            ExpressionError::at(
                format!("invalid expression syntax: bad number {text:?}"),
                source,
                position,
            )
        })?)
    } else {
        match text.parse::<i64>() {
            Ok(value) => Tok::Int(value),
            // Integers beyond i64 degrade to float rather than failing, which
            // keeps arithmetic on very large literals usable.
            Err(_) => Tok::Float(text.parse::<f64>().map_err(|_| {
                ExpressionError::at(
                    format!("invalid expression syntax: bad number {text:?}"),
                    source,
                    position,
                )
            })?),
        }
    };
    Ok((token, index - start))
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum CompareOp {
    Eq,
    NotEq,
    Lt,
    LtE,
    Gt,
    GtE,
    In,
    NotIn,
    Is,
    IsNot,
}

#[derive(Clone, Debug, PartialEq)]
enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
}

#[derive(Clone, Debug, PartialEq)]
enum Node {
    Constant(Value),
    Name(String, Position),
    Attribute(Box<Node>, String, Position),
    Subscript(Box<Node>, Box<Node>, Position),
    Slice(
        Option<Box<Node>>,
        Option<Box<Node>>,
        Option<Box<Node>>,
        Position,
    ),
    List(Vec<Node>),
    Dict(Vec<(Node, Node)>, Position),
    And(Vec<Node>, Position),
    Or(Vec<Node>, Position),
    Not(Box<Node>, Position),
    Unary(bool, Box<Node>, Position),
    Binary(BinOp, Box<Node>, Box<Node>, Position),
    Compare(Box<Node>, Vec<(CompareOp, Node)>, Position),
    IfExp(Box<Node>, Box<Node>, Box<Node>, Position),
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser<'a> {
    tokens: Vec<Token>,
    index: usize,
    source: &'a str,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> &Tok {
        &self.tokens[self.index].kind
    }

    fn position(&self) -> Position {
        self.tokens[self.index].position
    }

    fn advance(&mut self) -> Token {
        let token = self.tokens[self.index].clone();
        if self.index + 1 < self.tokens.len() {
            self.index += 1;
        }
        token
    }

    fn eat_op(&mut self, operator: &str) -> bool {
        if matches!(self.peek(), Tok::Op(found) if *found == operator) {
            self.advance();
            return true;
        }
        false
    }

    fn eat_keyword(&mut self, keyword: &str) -> bool {
        if matches!(self.peek(), Tok::Name(found) if found == keyword) {
            self.advance();
            return true;
        }
        false
    }

    fn at_keyword(&self, keyword: &str) -> bool {
        matches!(self.peek(), Tok::Name(found) if found == keyword)
    }

    fn expect_op(&mut self, operator: &str) -> Result<()> {
        if self.eat_op(operator) {
            return Ok(());
        }
        Err(self.error(format!(
            "invalid expression syntax: expected {operator:?}"
        )))
    }

    fn error(&self, message: impl Into<String>) -> ExpressionError {
        ExpressionError::at(message, self.source, self.position())
    }

    fn parse_expression(&mut self) -> Result<Node> {
        self.parse_conditional()
    }

    fn parse_conditional(&mut self) -> Result<Node> {
        let position = self.position();
        let body = self.parse_or()?;
        if self.at_keyword("if") {
            self.advance();
            let test = self.parse_or()?;
            if !self.eat_keyword("else") {
                return Err(self.error(
                    "invalid expression syntax: conditional expression requires else",
                ));
            }
            let orelse = self.parse_conditional()?;
            return Ok(Node::IfExp(
                Box::new(test),
                Box::new(body),
                Box::new(orelse),
                position,
            ));
        }
        Ok(body)
    }

    fn parse_or(&mut self) -> Result<Node> {
        let position = self.position();
        let mut values = vec![self.parse_and()?];
        while self.at_keyword("or") {
            self.advance();
            values.push(self.parse_and()?);
        }
        if values.len() == 1 {
            return Ok(values.pop().expect("one value"));
        }
        Ok(Node::Or(values, position))
    }

    fn parse_and(&mut self) -> Result<Node> {
        let position = self.position();
        let mut values = vec![self.parse_not()?];
        while self.at_keyword("and") {
            self.advance();
            values.push(self.parse_not()?);
        }
        if values.len() == 1 {
            return Ok(values.pop().expect("one value"));
        }
        Ok(Node::And(values, position))
    }

    fn parse_not(&mut self) -> Result<Node> {
        let position = self.position();
        if self.at_keyword("not") {
            self.advance();
            return Ok(Node::Not(Box::new(self.parse_not()?), position));
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> Result<Node> {
        let position = self.position();
        let left = self.parse_arithmetic()?;
        let mut comparators = Vec::new();
        loop {
            let operator = if self.eat_op("==") {
                CompareOp::Eq
            } else if self.eat_op("!=") {
                CompareOp::NotEq
            } else if self.eat_op("<=") {
                CompareOp::LtE
            } else if self.eat_op(">=") {
                CompareOp::GtE
            } else if self.eat_op("<") {
                CompareOp::Lt
            } else if self.eat_op(">") {
                CompareOp::Gt
            } else if self.at_keyword("in") {
                self.advance();
                CompareOp::In
            } else if self.at_keyword("is") {
                self.advance();
                if self.at_keyword("not") {
                    self.advance();
                    CompareOp::IsNot
                } else {
                    CompareOp::Is
                }
            } else if self.at_keyword("not") {
                // Only `not in` is a comparison operator here; a bare `not`
                // has already been handled at a lower precedence.
                let checkpoint = self.index;
                self.advance();
                if self.at_keyword("in") {
                    self.advance();
                    CompareOp::NotIn
                } else {
                    self.index = checkpoint;
                    break;
                }
            } else {
                break;
            };
            comparators.push((operator, self.parse_arithmetic()?));
        }
        if comparators.is_empty() {
            return Ok(left);
        }
        Ok(Node::Compare(Box::new(left), comparators, position))
    }

    fn parse_arithmetic(&mut self) -> Result<Node> {
        let mut left = self.parse_term()?;
        loop {
            let position = self.position();
            let operator = if self.eat_op("+") {
                BinOp::Add
            } else if self.eat_op("-") {
                BinOp::Sub
            } else {
                break;
            };
            let right = self.parse_term()?;
            left = Node::Binary(operator, Box::new(left), Box::new(right), position);
        }
        Ok(left)
    }

    fn parse_term(&mut self) -> Result<Node> {
        let mut left = self.parse_factor()?;
        loop {
            let position = self.position();
            let operator = if self.eat_op("//") {
                BinOp::FloorDiv
            } else if self.eat_op("*") {
                BinOp::Mul
            } else if self.eat_op("/") {
                BinOp::Div
            } else if self.eat_op("%") {
                BinOp::Mod
            } else {
                break;
            };
            let right = self.parse_factor()?;
            left = Node::Binary(operator, Box::new(left), Box::new(right), position);
        }
        Ok(left)
    }

    /// Unary +/- share a precedence level with `**`, so `-2 ** 2` is `-4`.
    fn parse_factor(&mut self) -> Result<Node> {
        let position = self.position();
        if self.eat_op("-") {
            return Ok(Node::Unary(true, Box::new(self.parse_factor()?), position));
        }
        if self.eat_op("+") {
            return Ok(Node::Unary(false, Box::new(self.parse_factor()?), position));
        }
        self.parse_power()
    }

    fn parse_power(&mut self) -> Result<Node> {
        let base = self.parse_trailer()?;
        let position = self.position();
        if self.eat_op("**") {
            let exponent = self.parse_factor()?;
            return Ok(Node::Binary(
                BinOp::Pow,
                Box::new(base),
                Box::new(exponent),
                position,
            ));
        }
        Ok(base)
    }

    fn parse_trailer(&mut self) -> Result<Node> {
        let mut node = self.parse_atom()?;
        loop {
            let position = self.position();
            if self.eat_op(".") {
                let Tok::Name(attribute) = self.advance().kind else {
                    return Err(self.error("invalid expression syntax: expected an attribute name"));
                };
                if attribute.starts_with('_') {
                    return Err(ExpressionError::at(
                        "attributes and mapping keys starting with '_' are not allowed",
                        self.source,
                        position,
                    ));
                }
                node = Node::Attribute(Box::new(node), attribute, position);
                continue;
            }
            if self.eat_op("[") {
                let index = self.parse_subscript_index()?;
                self.expect_op("]")?;
                node = Node::Subscript(Box::new(node), Box::new(index), position);
                continue;
            }
            if matches!(self.peek(), Tok::Op("(")) {
                return Err(ExpressionError::at(
                    "function and method calls are not allowed",
                    self.source,
                    position,
                ));
            }
            break;
        }
        Ok(node)
    }

    fn parse_subscript_index(&mut self) -> Result<Node> {
        let position = self.position();
        let lower = if matches!(self.peek(), Tok::Op(":")) {
            None
        } else {
            Some(Box::new(self.parse_expression()?))
        };
        if !self.eat_op(":") {
            return Ok(*lower.expect("index without a slice colon"));
        }
        let upper = if matches!(self.peek(), Tok::Op(":") | Tok::Op("]")) {
            None
        } else {
            Some(Box::new(self.parse_expression()?))
        };
        let step = if self.eat_op(":") {
            if matches!(self.peek(), Tok::Op("]")) {
                None
            } else {
                Some(Box::new(self.parse_expression()?))
            }
        } else {
            None
        };
        Ok(Node::Slice(lower, upper, step, position))
    }

    fn parse_atom(&mut self) -> Result<Node> {
        let position = self.position();
        match self.advance().kind {
            Tok::Int(value) => Ok(Node::Constant(Value::Number(value.into()))),
            Tok::Float(value) => Ok(Node::Constant(
                Number::from_f64(value)
                    .map(Value::Number)
                    .ok_or_else(|| self.error("unsupported literal: non-finite number"))?,
            )),
            Tok::Str(value) => Ok(Node::Constant(Value::String(value))),
            Tok::Name(name) => match name.as_str() {
                "True" => Ok(Node::Constant(Value::Bool(true))),
                "False" => Ok(Node::Constant(Value::Bool(false))),
                "None" => Ok(Node::Constant(Value::Null)),
                "lambda" => Err(ExpressionError::at(
                    "unsupported expression element: Lambda",
                    self.source,
                    position,
                )),
                "if" | "else" | "and" | "or" | "not" | "in" | "is" | "for" => {
                    Err(ExpressionError::at(
                        format!("invalid expression syntax: unexpected keyword {name:?}"),
                        self.source,
                        position,
                    ))
                }
                _ if name.starts_with("__") => Err(ExpressionError::at(
                    "dunder variable names are not allowed",
                    self.source,
                    position,
                )),
                _ => Ok(Node::Name(name, position)),
            },
            Tok::Op("(") => {
                let first = self.parse_expression()?;
                if self.eat_op(")") {
                    return Ok(first);
                }
                // A parenthesised tuple; a generator expression would have a
                // `for` here and is rejected by parse_sequence_tail.
                let mut elements = vec![first];
                self.parse_sequence_tail(&mut elements, ")")?;
                Ok(Node::List(elements))
            }
            Tok::Op("[") => {
                let mut elements = Vec::new();
                if !self.eat_op("]") {
                    elements.push(self.parse_expression()?);
                    self.parse_sequence_tail(&mut elements, "]")?;
                }
                Ok(Node::List(elements))
            }
            Tok::Op("{") => self.parse_dict(position),
            other => Err(ExpressionError::at(
                format!("invalid expression syntax: unexpected {other:?}"),
                self.source,
                position,
            )),
        }
    }

    fn parse_sequence_tail(&mut self, elements: &mut Vec<Node>, closing: &str) -> Result<()> {
        loop {
            if self.at_keyword("for") {
                return Err(self.error("unsupported expression element: comprehension"));
            }
            if self.eat_op(closing) {
                return Ok(());
            }
            if !self.eat_op(",") {
                return Err(self.error(format!(
                    "invalid expression syntax: expected {closing:?}"
                )));
            }
            if self.eat_op(closing) {
                return Ok(());
            }
            elements.push(self.parse_expression()?);
        }
    }

    fn parse_dict(&mut self, position: Position) -> Result<Node> {
        let mut entries = Vec::new();
        if self.eat_op("}") {
            return Ok(Node::Dict(entries, position));
        }
        loop {
            if self.eat_op("**") {
                return Err(self.error("dictionary unpacking is not allowed"));
            }
            let key = self.parse_expression()?;
            if self.at_keyword("for") {
                return Err(self.error("unsupported expression element: comprehension"));
            }
            // A `{a, b}` set literal has no colon and no JSON equivalent.
            self.expect_op(":")
                .map_err(|_| self.error("unsupported expression element: Set"))?;
            let value = self.parse_expression()?;
            if let Node::Constant(Value::String(text)) = &key {
                if is_dunder(text) {
                    return Err(ExpressionError::at(
                        "dunder keys are not allowed",
                        self.source,
                        position,
                    ));
                }
            }
            entries.push((key, value));
            if self.eat_op("}") {
                return Ok(Node::Dict(entries, position));
            }
            if !self.eat_op(",") {
                return Err(self.error("invalid expression syntax: expected \"}\""));
            }
            if self.eat_op("}") {
                return Ok(Node::Dict(entries, position));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// Python truthiness over the JSON value domain.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|value| value != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

fn number_value(value: f64) -> Result<Value> {
    Number::from_f64(value)
        .map(Value::Number)
        .ok_or_else(|| ExpressionError::new("operation failed: result is not a finite number"))
}

struct Evaluator<'a> {
    source: &'a str,
    variables: &'a Map<String, Value>,
}

impl<'a> Evaluator<'a> {
    fn error(&self, message: impl Into<String>, position: Position) -> ExpressionError {
        ExpressionError::at(message, self.source, position)
    }

    fn evaluate(&self, node: &Node) -> Result<Value> {
        match node {
            Node::Constant(value) => Ok(value.clone()),
            Node::Name(name, position) => self
                .variables
                .get(name)
                .cloned()
                .ok_or_else(|| self.error(format!("unknown variable: {name:?}"), *position)),
            Node::Attribute(target, attribute, position) => {
                let value = self.evaluate(target)?;
                // Mapping keys take precedence, matching the Python evaluator:
                // `user.items` reads the key, not a host-language method.
                if let Value::Object(map) = &value {
                    if let Some(found) = map.get(attribute) {
                        return Ok(found.clone());
                    }
                }
                Err(self.error(
                    format!("attribute or mapping key {attribute:?} was not found"),
                    *position,
                ))
            }
            Node::Subscript(target, index, position) => {
                let value = self.evaluate(target)?;
                if let Node::Slice(lower, upper, step, _) = index.as_ref() {
                    return self.evaluate_slice(&value, lower, upper, step, *position);
                }
                let key = self.evaluate(index)?;
                if let Value::String(text) = &key {
                    if is_dunder(text) {
                        return Err(self.error("dunder keys are not allowed", *position));
                    }
                }
                self.subscript(&value, &key, *position)
            }
            Node::Slice(_, _, _, position) => {
                Err(self.error("a slice is only valid inside a subscript", *position))
            }
            Node::List(elements) => Ok(Value::Array(
                elements
                    .iter()
                    .map(|element| self.evaluate(element))
                    .collect::<Result<Vec<_>>>()?,
            )),
            Node::Dict(entries, position) => {
                let mut map = Map::new();
                for (key_node, value_node) in entries {
                    let key = self.evaluate(key_node)?;
                    let Value::String(key) = key else {
                        return Err(self.error(
                            "invalid dictionary key: only string keys are supported",
                            *position,
                        ));
                    };
                    if is_dunder(&key) {
                        return Err(self.error("dunder keys are not allowed", *position));
                    }
                    map.insert(key, self.evaluate(value_node)?);
                }
                Ok(Value::Object(map))
            }
            Node::And(values, _) => {
                let mut result = self.evaluate(&values[0])?;
                for value in &values[1..] {
                    if !truthy(&result) {
                        return Ok(result);
                    }
                    result = self.evaluate(value)?;
                }
                Ok(result)
            }
            Node::Or(values, _) => {
                let mut result = self.evaluate(&values[0])?;
                for value in &values[1..] {
                    if truthy(&result) {
                        return Ok(result);
                    }
                    result = self.evaluate(value)?;
                }
                Ok(result)
            }
            Node::Not(operand, _) => Ok(Value::Bool(!truthy(&self.evaluate(operand)?))),
            Node::Unary(negate, operand, position) => {
                let value = self.evaluate(operand)?;
                let number = as_f64(&value)
                    .ok_or_else(|| self.error("operation failed: not a number", *position))?;
                if !negate {
                    return Ok(value);
                }
                match as_i64(&value) {
                    Some(integer) => Ok(Value::Number((-integer).into())),
                    None => number_value(-number),
                }
            }
            Node::Binary(operator, left, right, position) => {
                let left = self.evaluate(left)?;
                let right = self.evaluate(right)?;
                self.binary(operator, &left, &right, *position)
            }
            Node::Compare(left, comparators, position) => {
                let mut current = self.evaluate(left)?;
                for (operator, node) in comparators {
                    let right = self.evaluate(node)?;
                    if !self.compare(operator, &current, &right, *position)? {
                        return Ok(Value::Bool(false));
                    }
                    current = right;
                }
                Ok(Value::Bool(true))
            }
            Node::IfExp(test, body, orelse, _) => {
                let branch = if truthy(&self.evaluate(test)?) {
                    body
                } else {
                    orelse
                };
                self.evaluate(branch)
            }
        }
    }

    fn subscript(&self, value: &Value, key: &Value, position: Position) -> Result<Value> {
        match value {
            Value::Object(map) => {
                let Value::String(key) = key else {
                    return Err(self.error(
                        "subscript lookup failed: mapping keys must be strings",
                        position,
                    ));
                };
                map.get(key).cloned().ok_or_else(|| {
                    self.error(format!("subscript lookup failed: key {key:?}"), position)
                })
            }
            Value::Array(items) => {
                let index = as_i64(key).ok_or_else(|| {
                    self.error("subscript lookup failed: index must be an integer", position)
                })?;
                let resolved = if index < 0 {
                    items.len() as i64 + index
                } else {
                    index
                };
                if resolved < 0 || resolved as usize >= items.len() {
                    return Err(self.error("subscript lookup failed: index out of range", position));
                }
                Ok(items[resolved as usize].clone())
            }
            Value::String(text) => {
                let characters: Vec<char> = text.chars().collect();
                let index = as_i64(key).ok_or_else(|| {
                    self.error("subscript lookup failed: index must be an integer", position)
                })?;
                let resolved = if index < 0 {
                    characters.len() as i64 + index
                } else {
                    index
                };
                if resolved < 0 || resolved as usize >= characters.len() {
                    return Err(self.error("subscript lookup failed: index out of range", position));
                }
                Ok(Value::String(characters[resolved as usize].to_string()))
            }
            _ => Err(self.error("subscript lookup failed: value is not indexable", position)),
        }
    }

    fn evaluate_slice(
        &self,
        value: &Value,
        lower: &Option<Box<Node>>,
        upper: &Option<Box<Node>>,
        step: &Option<Box<Node>>,
        position: Position,
    ) -> Result<Value> {
        let bound = |node: &Option<Box<Node>>| -> Result<Option<i64>> {
            match node {
                None => Ok(None),
                Some(inner) => {
                    let evaluated = self.evaluate(inner)?;
                    if evaluated.is_null() {
                        return Ok(None);
                    }
                    as_i64(&evaluated).map(Some).ok_or_else(|| {
                        self.error("slice bounds must be integers", position)
                    })
                }
            }
        };
        let step = bound(step)?.unwrap_or(1);
        if step == 0 {
            return Err(self.error("operation failed: slice step cannot be zero", position));
        }
        let lower = bound(lower)?;
        let upper = bound(upper)?;

        match value {
            Value::Array(items) => {
                let selected = slice_indices(items.len(), lower, upper, step)
                    .into_iter()
                    .map(|index| items[index].clone())
                    .collect();
                Ok(Value::Array(selected))
            }
            Value::String(text) => {
                let characters: Vec<char> = text.chars().collect();
                let selected: String = slice_indices(characters.len(), lower, upper, step)
                    .into_iter()
                    .map(|index| characters[index])
                    .collect();
                Ok(Value::String(selected))
            }
            _ => Err(self.error("operation failed: value is not sliceable", position)),
        }
    }

    fn binary(
        &self,
        operator: &BinOp,
        left: &Value,
        right: &Value,
        position: Position,
    ) -> Result<Value> {
        // Sequence concatenation and repetition keep Python's behaviour for the
        // shapes that exist in the JSON domain.
        if matches!(operator, BinOp::Add) {
            match (left, right) {
                (Value::String(a), Value::String(b)) => {
                    return Ok(Value::String(format!("{a}{b}")))
                }
                (Value::Array(a), Value::Array(b)) => {
                    let mut joined = a.clone();
                    joined.extend(b.clone());
                    return Ok(Value::Array(joined));
                }
                _ => {}
            }
        }
        if matches!(operator, BinOp::Mul) {
            if let (Value::String(text), Some(count)) = (left, as_i64(right)) {
                return Ok(Value::String(text.repeat(count.max(0) as usize)));
            }
            if let (Some(count), Value::String(text)) = (as_i64(left), right) {
                return Ok(Value::String(text.repeat(count.max(0) as usize)));
            }
        }

        let (Some(a), Some(b)) = (as_f64(left), as_f64(right)) else {
            return Err(self.error(
                "operation failed: unsupported operand types".to_string(),
                position,
            ));
        };
        let integers = matches!(
            (as_i64(left), as_i64(right)),
            (Some(_), Some(_))
        ) && !matches!(left, Value::Number(n) if n.as_f64().is_some_and(|v| v.fract() != 0.0))
            && !matches!(right, Value::Number(n) if n.as_f64().is_some_and(|v| v.fract() != 0.0));

        match operator {
            BinOp::Add => {
                if integers {
                    let (a, b) = (as_i64(left).unwrap(), as_i64(right).unwrap());
                    return a
                        .checked_add(b)
                        .map(|value| Value::Number(value.into()))
                        .ok_or_else(|| self.error("operation failed: integer overflow", position));
                }
                number_value(a + b)
            }
            BinOp::Sub => {
                if integers {
                    let (a, b) = (as_i64(left).unwrap(), as_i64(right).unwrap());
                    return a
                        .checked_sub(b)
                        .map(|value| Value::Number(value.into()))
                        .ok_or_else(|| self.error("operation failed: integer overflow", position));
                }
                number_value(a - b)
            }
            BinOp::Mul => {
                if integers {
                    let (a, b) = (as_i64(left).unwrap(), as_i64(right).unwrap());
                    return a
                        .checked_mul(b)
                        .map(|value| Value::Number(value.into()))
                        .ok_or_else(|| self.error("operation failed: integer overflow", position));
                }
                number_value(a * b)
            }
            // True division always produces a float, as in Python 3.
            BinOp::Div => {
                if b == 0.0 {
                    return Err(self.error("operation failed: division by zero", position));
                }
                number_value(a / b)
            }
            BinOp::FloorDiv => {
                if b == 0.0 {
                    return Err(self.error("operation failed: division by zero", position));
                }
                let quotient = (a / b).floor();
                if integers {
                    return Ok(Value::Number((quotient as i64).into()));
                }
                number_value(quotient)
            }
            BinOp::Mod => {
                if b == 0.0 {
                    return Err(self.error("operation failed: modulo by zero", position));
                }
                // Python's modulo follows the sign of the divisor.
                let remainder = a - b * (a / b).floor();
                if integers {
                    return Ok(Value::Number((remainder as i64).into()));
                }
                number_value(remainder)
            }
            BinOp::Pow => {
                let result = a.powf(b);
                if integers && b >= 0.0 && result.fract() == 0.0 && result.abs() < 9e18 {
                    return Ok(Value::Number((result as i64).into()));
                }
                number_value(result)
            }
        }
    }

    fn compare(
        &self,
        operator: &CompareOp,
        left: &Value,
        right: &Value,
        position: Position,
    ) -> Result<bool> {
        match operator {
            CompareOp::Eq => Ok(values_equal(left, right)),
            CompareOp::NotEq => Ok(!values_equal(left, right)),
            // Identity has no separate meaning in the JSON domain; the shapes
            // that matter in practice are `x is None` and `x is True`.
            CompareOp::Is => Ok(values_equal(left, right)),
            CompareOp::IsNot => Ok(!values_equal(left, right)),
            CompareOp::In => self.contains(right, left, position),
            CompareOp::NotIn => Ok(!self.contains(right, left, position)?),
            _ => {
                let ordering = order(left, right).ok_or_else(|| {
                    self.error("operation failed: values are not orderable", position)
                })?;
                Ok(match operator {
                    CompareOp::Lt => ordering.is_lt(),
                    CompareOp::LtE => ordering.is_le(),
                    CompareOp::Gt => ordering.is_gt(),
                    CompareOp::GtE => ordering.is_ge(),
                    _ => unreachable!("handled above"),
                })
            }
        }
    }

    fn contains(&self, container: &Value, needle: &Value, position: Position) -> Result<bool> {
        match container {
            Value::Array(items) => Ok(items.iter().any(|item| values_equal(item, needle))),
            Value::Object(map) => match needle {
                Value::String(key) => Ok(map.contains_key(key)),
                _ => Ok(false),
            },
            Value::String(text) => match needle {
                Value::String(part) => Ok(text.contains(part.as_str())),
                _ => Err(self.error(
                    "operation failed: substring test requires a string",
                    position,
                )),
            },
            _ => Err(self.error(
                "operation failed: value does not support membership tests",
                position,
            )),
        }
    }
}

/// Resolve Python slice semantics into concrete indices.
fn slice_indices(length: usize, lower: Option<i64>, upper: Option<i64>, step: i64) -> Vec<usize> {
    let length = length as i64;
    let normalize = |value: i64| -> i64 {
        if value < 0 {
            (length + value).max(0)
        } else {
            value.min(length)
        }
    };
    let mut indices = Vec::new();
    if step > 0 {
        let start = lower.map_or(0, normalize);
        let stop = upper.map_or(length, normalize);
        let mut index = start;
        while index < stop {
            indices.push(index as usize);
            index += step;
        }
    } else {
        let start = lower.map_or(length - 1, |value| {
            if value < 0 {
                (length + value).max(-1)
            } else {
                value.min(length - 1)
            }
        });
        let stop = upper.map_or(-1, |value| {
            if value < 0 {
                (length + value).max(-1)
            } else {
                value.min(length)
            }
        });
        let mut index = start;
        while index > stop {
            if index >= 0 && index < length {
                indices.push(index as usize);
            }
            index += step;
        }
    }
    indices
}

/// Equality with Python's numeric cross-type behaviour (`1 == 1.0`, `True == 1`).
fn values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| values_equal(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| values_equal(value, other)))
        }
        (Value::String(a), Value::String(b)) => a == b,
        _ => match (as_f64(left), as_f64(right)) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        },
    }
}

fn order(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    if let (Some(a), Some(b)) = (as_f64(left), as_f64(right)) {
        return a.partial_cmp(&b);
    }
    match (left, right) {
        (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
        (Value::Array(a), Value::Array(b)) => {
            for (x, y) in a.iter().zip(b) {
                match order(x, y)? {
                    std::cmp::Ordering::Equal => continue,
                    other => return Some(other),
                }
            }
            Some(a.len().cmp(&b.len()))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A validated expression that can be evaluated repeatedly.
#[derive(Clone, Debug)]
pub struct CompiledExpression {
    source: String,
    tree: Node,
}

impl CompiledExpression {
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Evaluate using `variables` as the name context.
    pub fn evaluate(&self, variables: &Map<String, Value>) -> Result<Value> {
        Evaluator {
            source: &self.source,
            variables,
        }
        .evaluate(&self.tree)
    }

    /// Statically visible `steps.<id>` and `steps["id"]` references.
    ///
    /// The compiler uses this to prove that a step declared every sibling whose
    /// output it reads; dynamic keys cannot be proven here and are checked at
    /// run time instead.
    pub fn step_references(&self) -> Vec<String> {
        let mut found = Vec::new();
        collect_step_references(&self.tree, &mut found);
        found.sort();
        found.dedup();
        found
    }
}

fn collect_step_references(node: &Node, found: &mut Vec<String>) {
    match node {
        Node::Attribute(target, attribute, _) => {
            if matches!(target.as_ref(), Node::Name(name, _) if name == "steps") {
                found.push(attribute.clone());
            }
            collect_step_references(target, found);
        }
        Node::Subscript(target, index, _) => {
            if matches!(target.as_ref(), Node::Name(name, _) if name == "steps") {
                if let Node::Constant(Value::String(key)) = index.as_ref() {
                    found.push(key.clone());
                }
            }
            collect_step_references(target, found);
            collect_step_references(index, found);
        }
        Node::Slice(lower, upper, step, _) => {
            for part in [lower, upper, step].into_iter().flatten() {
                collect_step_references(part, found);
            }
        }
        Node::List(elements) => elements
            .iter()
            .for_each(|element| collect_step_references(element, found)),
        Node::Dict(entries, _) => entries.iter().for_each(|(key, value)| {
            collect_step_references(key, found);
            collect_step_references(value, found);
        }),
        Node::And(values, _) | Node::Or(values, _) => values
            .iter()
            .for_each(|value| collect_step_references(value, found)),
        Node::Not(inner, _) | Node::Unary(_, inner, _) => collect_step_references(inner, found),
        Node::Binary(_, left, right, _) => {
            collect_step_references(left, found);
            collect_step_references(right, found);
        }
        Node::Compare(left, comparators, _) => {
            collect_step_references(left, found);
            comparators
                .iter()
                .for_each(|(_, node)| collect_step_references(node, found));
        }
        Node::IfExp(test, body, orelse, _) => {
            collect_step_references(test, found);
            collect_step_references(body, found);
            collect_step_references(orelse, found);
        }
        Node::Constant(_) | Node::Name(_, _) => {}
    }
}

/// Parse and validate `source` without evaluating it.
pub fn compile_expression(source: &str) -> Result<CompiledExpression> {
    let tokens = tokenize(source)?;
    let mut parser = Parser {
        tokens,
        index: 0,
        source,
    };
    let tree = parser.parse_expression()?;
    if !matches!(parser.peek(), Tok::End) {
        return Err(parser.error("invalid expression syntax: unexpected trailing input"));
    }
    Ok(CompiledExpression {
        source: source.to_string(),
        tree,
    })
}

/// Compile and evaluate in one step.
pub fn evaluate_expression(source: &str, variables: &Map<String, Value>) -> Result<Value> {
    compile_expression(source)?.evaluate(variables)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn context(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn evaluate(source: &str, variables: Value) -> Result<Value> {
        evaluate_expression(source, &context(variables))
    }

    #[test]
    fn arithmetic_operators_match_python() {
        let variables = json!({ "amount": 6 });
        for (source, expected) in [
            ("2 + 3 * 4 - 5", json!(9)),
            ("20 / 4", json!(5.0)),
            ("17 // 5", json!(3)),
            ("17 % 5", json!(2)),
            ("2 ** 5", json!(32)),
            ("-(3 + 4)", json!(-7)),
            ("+amount", json!(6)),
        ] {
            assert_eq!(evaluate(source, variables.clone()).unwrap(), expected, "{source}");
        }
    }

    #[test]
    fn unary_minus_binds_looser_than_power() {
        assert_eq!(evaluate("-2 ** 2", json!({})).unwrap(), json!(-4));
    }

    #[test]
    fn compiled_expression_can_be_reused() {
        let expression = compile_expression("subtotal * (1 - discount)").unwrap();

        assert_eq!(
            expression
                .evaluate(&context(json!({"subtotal": 100, "discount": 0.2})))
                .unwrap(),
            json!(80.0)
        );
        assert_eq!(
            expression
                .evaluate(&context(json!({"subtotal": 50, "discount": 0.1})))
                .unwrap(),
            json!(45.0)
        );
    }

    #[test]
    fn boolean_comparisons_and_conditional_expression() {
        let variables = json!({
            "enabled": true, "disabled": false, "attempts": 2, "role": "admin"
        });

        assert_eq!(
            evaluate("enabled and attempts < 3 and role == 'admin'", variables.clone()).unwrap(),
            json!(true)
        );
        assert_eq!(
            evaluate("disabled or attempts >= 2", variables.clone()).unwrap(),
            json!(true)
        );
        assert_eq!(evaluate("not disabled", variables.clone()).unwrap(), json!(true));
        assert_eq!(
            evaluate("'allowed' if enabled else 'denied'", variables).unwrap(),
            json!("allowed")
        );
    }

    #[test]
    fn boolean_operators_short_circuit_before_unknown_names() {
        assert_eq!(evaluate("False and unknown_variable", json!({})).unwrap(), json!(false));
        assert_eq!(evaluate("True or unknown_variable", json!({})).unwrap(), json!(true));
    }

    #[test]
    fn comparison_chaining_evaluates_every_link() {
        let variables = json!({ "value": 5 });
        assert_eq!(evaluate("1 < value < 10", variables.clone()).unwrap(), json!(true));
        assert_eq!(evaluate("1 < value < 3", variables).unwrap(), json!(false));
    }

    #[test]
    fn mapping_attributes_and_subscripts() {
        let variables = json!({
            "user": {
                "profile": {"name": "Ada"},
                "roles": ["admin", "editor"],
                "items": "mapping value wins over dict.items"
            },
            "field": "name",
            "values": [10, 20, 30, 40, 50]
        });

        assert_eq!(evaluate("user.profile.name", variables.clone()).unwrap(), json!("Ada"));
        assert_eq!(
            evaluate("user['profile'][field]", variables.clone()).unwrap(),
            json!("Ada")
        );
        assert_eq!(evaluate("user.roles[1]", variables.clone()).unwrap(), json!("editor"));
        assert_eq!(
            evaluate("user.items", variables.clone()).unwrap(),
            json!("mapping value wins over dict.items")
        );
        assert_eq!(evaluate("values[1:5:2]", variables).unwrap(), json!([20, 40]));
    }

    #[test]
    fn slices_support_open_bounds_and_negative_steps() {
        let variables = json!({ "values": [10, 20, 30, 40, 50] });
        assert_eq!(evaluate("values[:2]", variables.clone()).unwrap(), json!([10, 20]));
        assert_eq!(evaluate("values[3:]", variables.clone()).unwrap(), json!([40, 50]));
        assert_eq!(evaluate("values[-2:]", variables.clone()).unwrap(), json!([40, 50]));
        assert_eq!(
            evaluate("values[::-1]", variables).unwrap(),
            json!([50, 40, 30, 20, 10])
        );
    }

    #[test]
    fn membership_tests_cover_arrays_objects_and_strings() {
        let variables = json!({
            "roles": ["admin"], "flags": {"beta": true}, "text": "hello world"
        });
        assert_eq!(evaluate("'admin' in roles", variables.clone()).unwrap(), json!(true));
        assert_eq!(evaluate("'beta' in flags", variables.clone()).unwrap(), json!(true));
        assert_eq!(evaluate("'world' in text", variables.clone()).unwrap(), json!(true));
        assert_eq!(evaluate("'nope' not in roles", variables).unwrap(), json!(true));
    }

    #[test]
    fn none_comparisons_use_is() {
        let variables = json!({ "value": null });
        assert_eq!(evaluate("value is None", variables.clone()).unwrap(), json!(true));
        assert_eq!(evaluate("value is not None", variables).unwrap(), json!(false));
    }

    #[test]
    fn builtin_names_are_not_implicitly_available() {
        assert!(evaluate("len", json!({})).is_err());
        assert_eq!(evaluate("len", json!({"len": 3})).unwrap(), json!(3));
    }

    #[test]
    fn function_and_method_calls_are_rejected() {
        for source in [
            "callback()",
            "open('/tmp/unsafe')",
            "max([1, 2])()",
            "text.upper()",
            "service.run('command')",
            "len(values)",
            "sorted(values)",
        ] {
            assert!(evaluate(source, json!({})).is_err(), "{source} must be rejected");
        }
    }

    #[test]
    fn lambdas_and_comprehensions_are_rejected() {
        for source in [
            "lambda value: value",
            "[value for value in values]",
            "{value for value in values}",
            "{value: value for value in values}",
            "(value for value in values)",
        ] {
            assert!(evaluate(source, json!({})).is_err(), "{source} must be rejected");
        }
    }

    #[test]
    fn dunder_access_is_rejected_in_every_position() {
        assert!(evaluate("value.__class__", json!({"value": {}})).is_err());
        assert!(evaluate("payload['__class__']", json!({"payload": {}})).is_err());
        assert!(evaluate("{'__class__': 1}", json!({})).is_err());
        assert!(evaluate("__builtins__", json!({})).is_err());
        assert!(evaluate("value._private", json!({"value": {}})).is_err());
    }

    #[test]
    fn a_dunder_key_reached_through_a_variable_is_rejected() {
        let variables = json!({"payload": {"__dict__": {}}, "key": "__dict__"});
        assert!(evaluate("payload[key]", variables).is_err());
    }

    #[test]
    fn unknown_variable_reports_its_position() {
        let error = evaluate("missing + 1", json!({"present": 1})).unwrap_err();

        assert_eq!(error.source.as_deref(), Some("missing + 1"));
        assert_eq!(error.lineno, Some(1));
        assert_eq!(error.col_offset, Some(0));
    }

    #[test]
    fn division_by_zero_is_a_structured_error() {
        let error = evaluate("1 / 0", json!({})).unwrap_err();
        assert!(error.message.contains("division by zero"), "{}", error.message);
    }

    #[test]
    fn step_references_are_discovered_statically() {
        let expression =
            compile_expression("steps.first.output.value + steps['second'].output.count").unwrap();
        assert_eq!(expression.step_references(), vec!["first", "second"]);
    }

    #[test]
    fn dynamic_step_keys_are_not_reported_as_static_references() {
        let expression = compile_expression("steps[name].output").unwrap();
        assert!(expression.step_references().is_empty());
    }
}
