use super::*;

// ============================================================================
// Polyglot AST Sandbox (Code Mode JS/TS + closure syntax)
// ============================================================================

#[derive(Clone, Debug)]
pub enum CodeModeError {
    SyntaxError(String),
    FuelExhausted(u64),
    CallDepthExceeded(usize),
    ExprDepthExceeded(usize),
    MemoryLimitExceeded(usize, usize),
    Timeout(u64),
    SchemaPreflightFailed(Vec<SchemaViolation>),
    MaxHostCallsExceeded(usize),
    GlobalOutboundBlocked(String),
    ApprovalRequired(PendingApproval),
    Cancelled(String),
    RuntimeError(String),
}

impl CodeModeError {
    pub fn feature_status(&self) -> FeatureStatus {
        match self {
            Self::FuelExhausted(_) => FeatureStatus::FuelExhausted,
            Self::MemoryLimitExceeded(_, _) => FeatureStatus::MemoryExhausted,
            Self::Timeout(_) => FeatureStatus::Timeout,
            Self::SchemaPreflightFailed(_) => FeatureStatus::SchemaPreflightFailed,
            Self::GlobalOutboundBlocked(_) => FeatureStatus::OutboundBlocked,
            Self::ApprovalRequired(_) => FeatureStatus::ApprovalRequired,
            _ => FeatureStatus::Error,
        }
    }

    pub fn error_code(&self) -> i32 {
        match self {
            Self::FuelExhausted(_) => FUEL_EXHAUSTED,
            Self::MemoryLimitExceeded(_, _) => MEMORY_EXHAUSTED,
            Self::Timeout(_) => TIMEOUT,
            Self::SchemaPreflightFailed(_) => SCHEMA_PREFLIGHT_FAILED,
            Self::CallDepthExceeded(_) | Self::ExprDepthExceeded(_) => FUEL_EXHAUSTED,
            Self::GlobalOutboundBlocked(_) => OUTBOUND_BLOCKED,
            Self::ApprovalRequired(_) => APPROVAL_REQUIRED,
            _ => INTERNAL_ERROR,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::SyntaxError(msg) => format!("SyntaxError: {}", msg),
            Self::FuelExhausted(max) => format!(
                "FuelExhausted: Script exceeded maximum allowed instruction limit ({} operations)",
                max
            ),
            Self::CallDepthExceeded(max) => {
                format!("CallDepthExceeded: Exceeded maximum stack depth ({})", max)
            }
            Self::ExprDepthExceeded(max) => format!(
                "ExprDepthExceeded: Exceeded maximum AST expression nesting depth ({})",
                max
            ),
            Self::MemoryLimitExceeded(used, max) => format!(
                "MemoryLimitExceeded: Script memory allocation ({} bytes) exceeded cap ({} bytes)",
                used, max
            ),
            Self::Timeout(max_ms) => format!(
                "Timeout: Script execution exceeded maximum wall-clock deadline ({} ms)",
                max_ms
            ),
            Self::SchemaPreflightFailed(violations) => format!(
                "SchemaPreflightFailed: {} argument violations detected (-32035 SCHEMA_PREFLIGHT_FAILED): {:?}",
                violations.len(),
                violations
            ),
            Self::MaxHostCallsExceeded(max) => format!(
                "MaxHostCallsExceeded: Script exceeded maximum allowed downstream tool calls ({})",
                max
            ),
            Self::GlobalOutboundBlocked(api) => format!(
                "GlobalOutboundBlocked: Direct outbound/system access via '{}' is forbidden in Code Mode sandbox (globalOutbound: null). Use governed codemode.<server>.<tool>() bindings.",
                api
            ),
            Self::ApprovalRequired(p) => format!(
                "ApprovalRequired: Tool '{}.{}' is marked as a state-mutating operation and requires approval.",
                p.server_name, p.tool_name
            ),
            Self::Cancelled(msg) => format!("Cancelled: {}", msg),
            Self::RuntimeError(msg) => format!("RuntimeError: {}", msg),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Token {
    Ident(String),
    Str(String),
    Template(Vec<(String, Option<String>)>),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    // Keywords
    Let,
    Mut,
    Const,
    Var,
    Async,
    Await,
    Function,
    For,
    In,
    Of,
    While,
    If,
    Else,
    Try,
    Catch,
    Finally,
    Throw,
    Return,
    Typeof,
    // Delimiters
    LParen,
    RParen,
    LBrace,
    HashBrace, // #{
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Colon,
    DoubleColon, // ::
    Semicolon,
    Dot,
    Question,
    Pipe,  // |
    Arrow, // =>
    // Operators
    Assign,  // =
    PlusEq,  // +=
    MinusEq, // -=
    StarEq,  // *=
    SlashEq, // /=
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,  // == or ===
    NotEq, // != or !==
    Lt,
    LtEq,
    Gt,
    GtEq,
    AndAnd,       // &&
    OrOr,         // ||
    NullCoalesce, // ??
    Bang,         // !
    Eof,
}

pub(super) struct Lexer<'a> {
    chars: Vec<char>,
    pos: usize,
    _src: &'a str,
}

impl<'a> Lexer<'a> {
    pub(super) fn new(src: &'a str) -> Self {
        Self {
            chars: src.chars().collect(),
            pos: 0,
            _src: src,
        }
    }

    pub(super) fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    pub(super) fn peek_next(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }

    pub(super) fn advance(&mut self) -> Option<char> {
        let ch = self.chars.get(self.pos).copied();
        if ch.is_some() {
            self.pos += 1;
        }
        ch
    }

    pub(super) fn tokenize(&mut self) -> Result<Vec<Token>, CodeModeError> {
        let mut tokens = Vec::new();
        while let Some(ch) = self.peek() {
            if ch.is_whitespace() {
                self.advance();
                continue;
            }
            // Comments
            if ch == '/' && self.peek_next() == Some('/') {
                while let Some(c) = self.advance() {
                    if c == '\n' {
                        break;
                    }
                }
                continue;
            }
            if ch == '/' && self.peek_next() == Some('*') {
                self.advance();
                self.advance();
                while let Some(c) = self.advance() {
                    if c == '*' && self.peek() == Some('/') {
                        self.advance();
                        break;
                    }
                }
                continue;
            }

            // Template literals (`...${expr}...`)
            if ch == '`' {
                self.advance();
                let mut parts: Vec<(String, Option<String>)> = Vec::new();
                let mut lit = String::new();
                let mut terminated = false;
                while let Some(c) = self.advance() {
                    if c == '`' {
                        terminated = true;
                        break;
                    }
                    if c == '\\' {
                        if let Some(esc) = self.advance() {
                            match esc {
                                'n' => lit.push('\n'),
                                'r' => lit.push('\r'),
                                't' => lit.push('\t'),
                                '\\' => lit.push('\\'),
                                '`' => lit.push('`'),
                                '$' => lit.push('$'),
                                other => lit.push(other),
                            }
                        }
                    } else if c == '$' && self.peek() == Some('{') {
                        self.advance(); // '{'
                        let mut expr_src = String::new();
                        let mut depth = 1usize;
                        while let Some(ec) = self.advance() {
                            if ec == '{' {
                                depth += 1;
                                expr_src.push(ec);
                            } else if ec == '}' {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                                expr_src.push(ec);
                            } else {
                                expr_src.push(ec);
                            }
                        }
                        if depth != 0 {
                            return Err(CodeModeError::SyntaxError(
                                "Unterminated interpolation in template literal".to_string(),
                            ));
                        }
                        parts.push((std::mem::take(&mut lit), Some(expr_src)));
                    } else {
                        lit.push(c);
                    }
                }
                if !terminated {
                    return Err(CodeModeError::SyntaxError(
                        "Unterminated template literal".to_string(),
                    ));
                }
                if !lit.is_empty() || parts.is_empty() {
                    parts.push((lit, None));
                }
                tokens.push(Token::Template(parts));
                continue;
            }

            // String literals
            if ch == '"' || ch == '\'' {
                let quote = self.advance().unwrap();
                let mut s = String::new();
                let mut terminated = false;
                while let Some(c) = self.advance() {
                    if c == quote {
                        terminated = true;
                        break;
                    }
                    if c == '\\' {
                        if let Some(esc) = self.advance() {
                            match esc {
                                'n' => s.push('\n'),
                                'r' => s.push('\r'),
                                't' => s.push('\t'),
                                '\\' => s.push('\\'),
                                '"' => s.push('"'),
                                '\'' => s.push('\''),
                                other => s.push(other),
                            }
                        }
                    } else {
                        s.push(c);
                    }
                }
                if !terminated {
                    return Err(CodeModeError::SyntaxError(
                        "Unterminated string literal".to_string(),
                    ));
                }
                tokens.push(Token::Str(s));
                continue;
            }

            // Numbers
            if ch.is_ascii_digit() {
                let mut num_str = String::new();
                let mut is_float = false;
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() {
                        num_str.push(c);
                        self.advance();
                    } else if c == '.'
                        && !is_float
                        && self
                            .peek_next()
                            .map(|n| n.is_ascii_digit())
                            .unwrap_or(false)
                    {
                        is_float = true;
                        num_str.push(c);
                        self.advance();
                    } else if c == '_' {
                        self.advance();
                    } else {
                        break;
                    }
                }
                if is_float {
                    let f = num_str.parse::<f64>().map_err(|_| {
                        CodeModeError::SyntaxError(format!("Invalid float literal '{}'", num_str))
                    })?;
                    tokens.push(Token::Float(f));
                } else {
                    let i = num_str.parse::<i64>().map_err(|_| {
                        CodeModeError::SyntaxError(format!("Invalid integer literal '{}'", num_str))
                    })?;
                    tokens.push(Token::Int(i));
                }
                continue;
            }

            // Identifiers & Keywords
            if ch.is_ascii_alphabetic() || ch == '_' || ch == '$' {
                let mut ident = String::new();
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                        ident.push(c);
                        self.advance();
                    } else {
                        break;
                    }
                }
                let tok = match ident.as_str() {
                    "let" => Token::Let,
                    "mut" => Token::Mut,
                    "const" => Token::Const,
                    "var" => Token::Var,
                    "async" => Token::Async,
                    "await" => Token::Await,
                    "function" | "fn" => Token::Function,
                    "for" => Token::For,
                    "in" => Token::In,
                    "of" => Token::Of,
                    "while" => Token::While,
                    "if" => Token::If,
                    "else" => Token::Else,
                    "try" => Token::Try,
                    "catch" => Token::Catch,
                    "finally" => Token::Finally,
                    "throw" => Token::Throw,
                    "return" => Token::Return,
                    "typeof" => Token::Typeof,
                    "true" => Token::Bool(true),
                    "false" => Token::Bool(false),
                    "null" | "undefined" => Token::Null,
                    _ => Token::Ident(ident),
                };
                tokens.push(tok);
                continue;
            }

            // Multi-char operators & punctuation
            match ch {
                '#' if self.peek_next() == Some('{') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::HashBrace);
                }
                ':' if self.peek_next() == Some(':') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::DoubleColon);
                }
                '=' if self.peek_next() == Some('>') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::Arrow);
                }
                '=' if self.peek_next() == Some('=') => {
                    self.advance();
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                    }
                    tokens.push(Token::EqEq);
                }
                '!' if self.peek_next() == Some('=') => {
                    self.advance();
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                    }
                    tokens.push(Token::NotEq);
                }
                '<' if self.peek_next() == Some('=') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::LtEq);
                }
                '>' if self.peek_next() == Some('=') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::GtEq);
                }
                '&' if self.peek_next() == Some('&') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::AndAnd);
                }
                '|' if self.peek_next() == Some('|') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::OrOr);
                }
                '?' if self.peek_next() == Some('?') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::NullCoalesce);
                }
                '+' if self.peek_next() == Some('=') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::PlusEq);
                }
                '-' if self.peek_next() == Some('=') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::MinusEq);
                }
                '*' if self.peek_next() == Some('=') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::StarEq);
                }
                '/' if self.peek_next() == Some('=') => {
                    self.advance();
                    self.advance();
                    tokens.push(Token::SlashEq);
                }
                '(' => {
                    self.advance();
                    tokens.push(Token::LParen);
                }
                ')' => {
                    self.advance();
                    tokens.push(Token::RParen);
                }
                '{' => {
                    self.advance();
                    tokens.push(Token::LBrace);
                }
                '}' => {
                    self.advance();
                    tokens.push(Token::RBrace);
                }
                '[' => {
                    self.advance();
                    tokens.push(Token::LBracket);
                }
                ']' => {
                    self.advance();
                    tokens.push(Token::RBracket);
                }
                ',' => {
                    self.advance();
                    tokens.push(Token::Comma);
                }
                ':' => {
                    self.advance();
                    tokens.push(Token::Colon);
                }
                ';' => {
                    self.advance();
                    tokens.push(Token::Semicolon);
                }
                '.' => {
                    self.advance();
                    tokens.push(Token::Dot);
                }
                '?' => {
                    self.advance();
                    tokens.push(Token::Question);
                }
                '|' => {
                    self.advance();
                    tokens.push(Token::Pipe);
                }
                '=' => {
                    self.advance();
                    tokens.push(Token::Assign);
                }
                '+' => {
                    self.advance();
                    tokens.push(Token::Plus);
                }
                '-' => {
                    self.advance();
                    tokens.push(Token::Minus);
                }
                '*' => {
                    self.advance();
                    tokens.push(Token::Star);
                }
                '/' => {
                    self.advance();
                    tokens.push(Token::Slash);
                }
                '%' => {
                    self.advance();
                    tokens.push(Token::Percent);
                }
                '<' => {
                    self.advance();
                    tokens.push(Token::Lt);
                }
                '>' => {
                    self.advance();
                    tokens.push(Token::Gt);
                }
                '!' => {
                    self.advance();
                    tokens.push(Token::Bang);
                }
                other => {
                    return Err(CodeModeError::SyntaxError(format!(
                        "Unexpected character '{}'",
                        other
                    )))
                }
            }
        }
        tokens.push(Token::Eof);
        Ok(tokens)
    }
}

#[derive(Clone, Debug)]
pub(super) struct ParallelTask {
    pub(super) target_var: Option<String>,
    pub(super) server_name: String,
    pub(super) tool_name: String,
    pub(super) args: Vec<Expr>,
}

#[derive(Clone, Debug)]
pub(super) enum Stmt {
    Let {
        name: String,
        expr: Expr,
    },
    LetDestructureObject {
        bindings: Vec<(String, String)>,
        expr: Expr,
    },
    LetDestructureArray {
        names: Vec<String>,
        expr: Expr,
    },
    Assign {
        target: AssignTarget,
        op: AssignOp,
        expr: Expr,
    },
    If {
        cond: Expr,
        then_branch: Vec<Stmt>,
        else_branch: Option<Vec<Stmt>>,
    },
    For {
        var: String,
        iterable: Expr,
        body: Vec<Stmt>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    TryCatch {
        try_body: Vec<Stmt>,
        catch_var: Option<String>,
        catch_body: Option<Vec<Stmt>>,
        finally_body: Option<Vec<Stmt>>,
    },
    Throw(Expr),
    ParallelWave(Vec<ParallelTask>),
    Return(Option<Expr>),
    Expr(Expr),
}

#[derive(Clone, Debug)]
pub(super) enum AssignTarget {
    Var(String),
    Field(String, String),
}

#[derive(Clone, Debug, Copy)]
pub(super) enum AssignOp {
    Set,
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Clone, Debug)]
pub(super) enum Expr {
    Literal(JsonValue),
    Template(Vec<(String, Option<Expr>)>),
    Var(String),
    Array(Vec<Expr>),
    Object(Vec<(String, Expr)>),
    Field(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Unary(UnaryOp, Box<Expr>),
    Binary(Box<Expr>, BinOp, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// Namespace or global call: e.g. `rides_service::list_recent(...)` or `fetch(...)`
    NamespaceCall {
        namespace: Option<String>,
        func: String,
        args: Vec<Expr>,
    },
    /// Method or chained call: e.g. `codemode.rides_service.list_recent(...)` or `arr.filter(...)`
    MethodCall {
        receiver: Box<Expr>,
        method: String,
        args: Vec<Expr>,
    },
    /// Closure / Arrow function: `|x| expr` or `x => expr` or `(x, y) => { ... }`
    Closure {
        params: Vec<String>,
        body: ClosureBody,
    },
}

#[derive(Clone, Debug)]
pub(super) enum ClosureBody {
    Expr(Box<Expr>),
    Block(Vec<Stmt>),
}

#[derive(Clone, Debug, Copy)]
pub(super) enum UnaryOp {
    Not,
    Neg,
    Typeof,
}

#[derive(Clone, Debug, Copy)]
pub(super) enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    NullCoalesce,
}

pub(super) struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    max_expr_depth: usize,
}

impl Parser {
    pub(super) fn new(tokens: Vec<Token>, max_expr_depth: usize) -> Self {
        Self {
            tokens,
            pos: 0,
            max_expr_depth,
        }
    }

    pub(super) fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    pub(super) fn peek_at(&self, offset: usize) -> &Token {
        self.tokens.get(self.pos + offset).unwrap_or(&Token::Eof)
    }

    pub(super) fn advance(&mut self) -> Token {
        let tok = self.peek().clone();
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    pub(super) fn expect(&mut self, expected: &Token) -> Result<(), CodeModeError> {
        let actual = self.advance();
        if &actual == expected {
            Ok(())
        } else {
            Err(CodeModeError::SyntaxError(format!(
                "Expected {:?}, found {:?}",
                expected, actual
            )))
        }
    }

    pub(super) fn consume_optional_semicolons(&mut self) {
        while self.peek() == &Token::Semicolon {
            self.advance();
        }
    }

    pub(super) fn parse_program(&mut self) -> Result<Vec<Stmt>, CodeModeError> {
        self.consume_optional_semicolons();

        // Unwrap top-level Code Mode wrapper if present:
        // e.g. `async () => { ... }`, `() => { ... }`, `async function run() { ... }`
        if self.peek() == &Token::Async {
            self.advance();
        }
        if self.peek() == &Token::Function {
            self.advance();
            if let Token::Ident(_) = self.peek() {
                self.advance();
            }
            if self.peek() == &Token::LParen {
                self.advance();
                while self.peek() != &Token::RParen && self.peek() != &Token::Eof {
                    self.advance();
                }
                let _ = self.expect(&Token::RParen);
            }
            if self.peek() == &Token::LBrace {
                return self.parse_block();
            }
        } else if self.peek() == &Token::LParen
            && self.peek_at(1) == &Token::RParen
            && self.peek_at(2) == &Token::Arrow
        {
            self.advance(); // (
            self.advance(); // )
            self.advance(); // =>
            if self.peek() == &Token::LBrace {
                let stmts = self.parse_block()?;
                self.consume_optional_semicolons();
                // Allow trailing `()` if wrapped like `(async () => { ... })()`
                if self.peek() == &Token::LParen && self.peek_at(1) == &Token::RParen {
                    self.advance();
                    self.advance();
                }
                return Ok(stmts);
            } else {
                let expr = self.parse_expr(0)?;
                return Ok(vec![Stmt::Return(Some(expr))]);
            }
        }

        let mut stmts = Vec::new();
        while self.peek() != &Token::Eof && self.peek() != &Token::RBrace {
            stmts.push(self.parse_stmt()?);
            self.consume_optional_semicolons();
        }
        Ok(stmts)
    }

    pub(super) fn parse_block(&mut self) -> Result<Vec<Stmt>, CodeModeError> {
        self.expect(&Token::LBrace)?;
        self.consume_optional_semicolons();
        let mut stmts = Vec::new();
        while self.peek() != &Token::RBrace && self.peek() != &Token::Eof {
            stmts.push(self.parse_stmt()?);
            self.consume_optional_semicolons();
        }
        self.expect(&Token::RBrace)?;
        Ok(stmts)
    }

    pub(super) fn parse_stmt(&mut self) -> Result<Stmt, CodeModeError> {
        match self.peek() {
            Token::Let | Token::Const | Token::Var => {
                self.advance();
                if self.peek() == &Token::Mut {
                    self.advance();
                }
                if self.peek() == &Token::LBrace {
                    self.advance(); // {
                    let mut bindings = Vec::new();
                    while self.peek() != &Token::RBrace && self.peek() != &Token::Eof {
                        let key = match self.advance() {
                            Token::Ident(s) => s,
                            other => {
                                return Err(CodeModeError::SyntaxError(format!(
                                    "Expected identifier in object destructuring, got {:?}",
                                    other
                                )))
                            }
                        };
                        let local = if self.peek() == &Token::Colon {
                            self.advance();
                            match self.advance() {
                                Token::Ident(s) => s,
                                other => {
                                    return Err(CodeModeError::SyntaxError(format!(
                                        "Expected alias identifier after ':', got {:?}",
                                        other
                                    )))
                                }
                            }
                        } else {
                            key.clone()
                        };
                        bindings.push((key, local));
                        if self.peek() == &Token::Comma {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                    self.expect(&Token::RBrace)?;
                    self.expect(&Token::Assign)?;
                    let expr = self.parse_expr(0)?;
                    self.consume_optional_semicolons();
                    return Ok(Stmt::LetDestructureObject { bindings, expr });
                }
                if self.peek() == &Token::LBracket {
                    self.advance(); // [
                    let mut names = Vec::new();
                    while self.peek() != &Token::RBracket && self.peek() != &Token::Eof {
                        let name = match self.advance() {
                            Token::Ident(s) => s,
                            other => {
                                return Err(CodeModeError::SyntaxError(format!(
                                    "Expected identifier in array destructuring, got {:?}",
                                    other
                                )))
                            }
                        };
                        names.push(name);
                        if self.peek() == &Token::Comma {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                    self.expect(&Token::RBracket)?;
                    self.expect(&Token::Assign)?;
                    let expr = self.parse_expr(0)?;
                    self.consume_optional_semicolons();
                    return Ok(Stmt::LetDestructureArray { names, expr });
                }
                let name = match self.advance() {
                    Token::Ident(s) => s,
                    other => {
                        return Err(CodeModeError::SyntaxError(format!(
                            "Expected variable name after let/const, got {:?}",
                            other
                        )))
                    }
                };
                // Optional TypeScript type annotation: `let x: number = ...`
                if self.peek() == &Token::Colon {
                    self.advance();
                    if let Token::Ident(_) = self.peek() {
                        self.advance();
                    }
                }
                let expr = if self.peek() == &Token::Assign {
                    self.advance();
                    self.parse_expr(0)?
                } else {
                    Expr::Literal(JsonValue::Null)
                };
                self.consume_optional_semicolons();
                Ok(Stmt::Let { name, expr })
            }
            Token::Try => {
                self.advance();
                let try_body = self.parse_block()?;
                let mut catch_var = None;
                let mut catch_body = None;
                let mut finally_body = None;
                if self.peek() == &Token::Catch {
                    self.advance();
                    if self.peek() == &Token::LParen {
                        self.advance();
                        if let Token::Ident(v) = self.advance() {
                            catch_var = Some(v);
                        }
                        self.expect(&Token::RParen)?;
                    }
                    catch_body = Some(self.parse_block()?);
                }
                if self.peek() == &Token::Finally {
                    self.advance();
                    finally_body = Some(self.parse_block()?);
                }
                if catch_body.is_none() && finally_body.is_none() {
                    return Err(CodeModeError::SyntaxError(
                        "try block requires catch or finally".to_string(),
                    ));
                }
                Ok(Stmt::TryCatch {
                    try_body,
                    catch_var,
                    catch_body,
                    finally_body,
                })
            }
            Token::Throw => {
                self.advance();
                let expr = self.parse_expr(0)?;
                self.consume_optional_semicolons();
                Ok(Stmt::Throw(expr))
            }
            Token::If => {
                self.advance();
                let cond = self.parse_expr(0)?;
                let then_branch = self.parse_block()?;
                let else_branch = if self.peek() == &Token::Else {
                    self.advance();
                    if self.peek() == &Token::If {
                        Some(vec![self.parse_stmt()?])
                    } else {
                        Some(self.parse_block()?)
                    }
                } else {
                    None
                };
                Ok(Stmt::If {
                    cond,
                    then_branch,
                    else_branch,
                })
            }
            Token::For => {
                self.advance();
                let has_paren = if self.peek() == &Token::LParen {
                    self.advance();
                    true
                } else {
                    false
                };
                if matches!(self.peek(), Token::Let | Token::Const | Token::Var) {
                    self.advance();
                }
                if self.peek() == &Token::Mut {
                    self.advance();
                }
                let var = match self.advance() {
                    Token::Ident(s) => s,
                    other => {
                        return Err(CodeModeError::SyntaxError(format!(
                            "Expected loop variable in 'for', got {:?}",
                            other
                        )))
                    }
                };
                match self.advance() {
                    Token::In | Token::Of => {}
                    other => {
                        return Err(CodeModeError::SyntaxError(format!(
                            "Expected 'in' or 'of' in for-loop, got {:?}",
                            other
                        )))
                    }
                }
                let iterable = self.parse_expr(0)?;
                if has_paren {
                    self.expect(&Token::RParen)?;
                }
                let body = self.parse_block()?;
                Ok(Stmt::For {
                    var,
                    iterable,
                    body,
                })
            }
            Token::While => {
                self.advance();
                let cond = self.parse_expr(0)?;
                let body = self.parse_block()?;
                Ok(Stmt::While { cond, body })
            }
            Token::Return => {
                self.advance();
                if matches!(self.peek(), Token::Semicolon | Token::RBrace | Token::Eof) {
                    self.consume_optional_semicolons();
                    Ok(Stmt::Return(None))
                } else {
                    let expr = self.parse_expr(0)?;
                    self.consume_optional_semicolons();
                    Ok(Stmt::Return(Some(expr)))
                }
            }
            _ => {
                // Check for assignment: `ident = expr` or `ident.field = expr`
                if let Token::Ident(name) = self.peek().clone() {
                    if let Some(op) = Self::token_to_assign_op(self.peek_at(1)) {
                        self.advance(); // ident
                        self.advance(); // op
                        let expr = self.parse_expr(0)?;
                        self.consume_optional_semicolons();
                        return Ok(Stmt::Assign {
                            target: AssignTarget::Var(name),
                            op,
                            expr,
                        });
                    }
                    if self.peek_at(1) == &Token::Dot {
                        if let Token::Ident(field) = self.peek_at(2).clone() {
                            if let Some(op) = Self::token_to_assign_op(self.peek_at(3)) {
                                self.advance(); // ident
                                self.advance(); // .
                                self.advance(); // field
                                self.advance(); // op
                                let expr = self.parse_expr(0)?;
                                self.consume_optional_semicolons();
                                return Ok(Stmt::Assign {
                                    target: AssignTarget::Field(name, field),
                                    op,
                                    expr,
                                });
                            }
                        }
                    }
                }
                let expr = self.parse_expr(0)?;
                self.consume_optional_semicolons();
                Ok(Stmt::Expr(expr))
            }
        }
    }

    pub(super) fn token_to_assign_op(tok: &Token) -> Option<AssignOp> {
        match tok {
            Token::Assign => Some(AssignOp::Set),
            Token::PlusEq => Some(AssignOp::Add),
            Token::MinusEq => Some(AssignOp::Sub),
            Token::StarEq => Some(AssignOp::Mul),
            Token::SlashEq => Some(AssignOp::Div),
            _ => None,
        }
    }

    pub(super) fn parse_expr(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        if depth > self.max_expr_depth {
            return Err(CodeModeError::ExprDepthExceeded(self.max_expr_depth));
        }
        self.parse_ternary(depth + 1)
    }

    pub(super) fn parse_ternary(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let cond = self.parse_null_coalesce(depth)?;
        if self.peek() == &Token::Question {
            self.advance();
            let then_expr = self.parse_expr(depth + 1)?;
            self.expect(&Token::Colon)?;
            let else_expr = self.parse_expr(depth + 1)?;
            return Ok(Expr::Ternary(
                Box::new(cond),
                Box::new(then_expr),
                Box::new(else_expr),
            ));
        }
        Ok(cond)
    }

    pub(super) fn parse_null_coalesce(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let mut left = self.parse_or(depth)?;
        while self.peek() == &Token::NullCoalesce {
            self.advance();
            let right = self.parse_or(depth)?;
            left = Expr::Binary(Box::new(left), BinOp::NullCoalesce, Box::new(right));
        }
        Ok(left)
    }

    pub(super) fn parse_or(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let mut left = self.parse_and(depth)?;
        while self.peek() == &Token::OrOr {
            self.advance();
            let right = self.parse_and(depth)?;
            left = Expr::Binary(Box::new(left), BinOp::Or, Box::new(right));
        }
        Ok(left)
    }

    pub(super) fn parse_and(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let mut left = self.parse_equality(depth)?;
        while self.peek() == &Token::AndAnd {
            self.advance();
            let right = self.parse_equality(depth)?;
            left = Expr::Binary(Box::new(left), BinOp::And, Box::new(right));
        }
        Ok(left)
    }

    pub(super) fn parse_equality(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let mut left = self.parse_comparison(depth)?;
        loop {
            let op = match self.peek() {
                Token::EqEq => BinOp::Eq,
                Token::NotEq => BinOp::NotEq,
                _ => break,
            };
            self.advance();
            let right = self.parse_comparison(depth)?;
            left = Expr::Binary(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    pub(super) fn parse_comparison(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let mut left = self.parse_additive(depth)?;
        loop {
            let op = match self.peek() {
                Token::Lt => BinOp::Lt,
                Token::LtEq => BinOp::LtEq,
                Token::Gt => BinOp::Gt,
                Token::GtEq => BinOp::GtEq,
                _ => break,
            };
            self.advance();
            let right = self.parse_additive(depth)?;
            left = Expr::Binary(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    pub(super) fn parse_additive(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let mut left = self.parse_multiplicative(depth)?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinOp::Add,
                Token::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_multiplicative(depth)?;
            left = Expr::Binary(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    pub(super) fn parse_multiplicative(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let mut left = self.parse_unary(depth)?;
        loop {
            let op = match self.peek() {
                Token::Star => BinOp::Mul,
                Token::Slash => BinOp::Div,
                Token::Percent => BinOp::Rem,
                _ => break,
            };
            self.advance();
            let right = self.parse_unary(depth)?;
            left = Expr::Binary(Box::new(left), op, Box::new(right));
        }
        Ok(left)
    }

    pub(super) fn parse_unary(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        if depth > self.max_expr_depth {
            return Err(CodeModeError::ExprDepthExceeded(self.max_expr_depth));
        }
        match self.peek() {
            Token::Await => {
                self.advance();
                self.parse_unary(depth + 1)
            }
            Token::Bang => {
                self.advance();
                let inner = self.parse_unary(depth + 1)?;
                Ok(Expr::Unary(UnaryOp::Not, Box::new(inner)))
            }
            Token::Minus => {
                self.advance();
                let inner = self.parse_unary(depth + 1)?;
                Ok(Expr::Unary(UnaryOp::Neg, Box::new(inner)))
            }
            Token::Typeof => {
                self.advance();
                let inner = self.parse_unary(depth + 1)?;
                Ok(Expr::Unary(UnaryOp::Typeof, Box::new(inner)))
            }
            _ => self.parse_postfix(depth + 1),
        }
    }

    pub(super) fn parse_postfix(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        let mut expr = self.parse_primary(depth)?;
        loop {
            match self.peek() {
                Token::Dot => {
                    self.advance();
                    let member = match self.advance() {
                        Token::Ident(s) => s,
                        other => {
                            return Err(CodeModeError::SyntaxError(format!(
                                "Expected property or method name after '.', got {:?}",
                                other
                            )))
                        }
                    };
                    if self.peek() == &Token::LParen {
                        let args = self.parse_arg_list(depth)?;
                        expr = Expr::MethodCall {
                            receiver: Box::new(expr),
                            method: member,
                            args,
                        };
                    } else {
                        expr = Expr::Field(Box::new(expr), member);
                    }
                }
                Token::LBracket => {
                    self.advance();
                    let idx = self.parse_expr(depth + 1)?;
                    self.expect(&Token::RBracket)?;
                    expr = Expr::Index(Box::new(expr), Box::new(idx));
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    pub(super) fn parse_arg_list(&mut self, depth: usize) -> Result<Vec<Expr>, CodeModeError> {
        self.expect(&Token::LParen)?;
        let mut args = Vec::new();
        while self.peek() != &Token::RParen && self.peek() != &Token::Eof {
            args.push(self.parse_expr(depth + 1)?);
            if self.peek() == &Token::Comma {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(&Token::RParen)?;
        Ok(args)
    }

    pub(super) fn parse_closure_body(
        &mut self,
        depth: usize,
    ) -> Result<ClosureBody, CodeModeError> {
        if self.peek() == &Token::LBrace {
            let stmts = self.parse_block()?;
            Ok(ClosureBody::Block(stmts))
        } else {
            let expr = self.parse_expr(depth + 1)?;
            Ok(ClosureBody::Expr(Box::new(expr)))
        }
    }

    pub(super) fn parse_primary(&mut self, depth: usize) -> Result<Expr, CodeModeError> {
        if depth > self.max_expr_depth {
            return Err(CodeModeError::ExprDepthExceeded(self.max_expr_depth));
        }

        // Closure: `|x| expr` or `|a, b| expr`
        if self.peek() == &Token::Pipe {
            self.advance();
            let mut params = Vec::new();
            while self.peek() != &Token::Pipe && self.peek() != &Token::Eof {
                if let Token::Ident(p) = self.advance() {
                    params.push(p);
                }
                if self.peek() == &Token::Comma {
                    self.advance();
                }
            }
            self.expect(&Token::Pipe)?;
            let body = self.parse_closure_body(depth)?;
            return Ok(Expr::Closure { params, body });
        }

        // JS single-param arrow function: `x => expr`
        if let Token::Ident(param) = self.peek().clone() {
            if self.peek_at(1) == &Token::Arrow {
                self.advance(); // ident
                self.advance(); // =>
                let body = self.parse_closure_body(depth)?;
                return Ok(Expr::Closure {
                    params: vec![param],
                    body,
                });
            }
        }

        // Parenthesized expression OR JS arrow function `(a, b) => ...`
        if self.peek() == &Token::LParen {
            // Check if this is `(p1, p2) => ...`
            let saved_pos = self.pos;
            self.advance(); // (
            let mut params = Vec::new();
            let mut is_arrow = true;
            while self.peek() != &Token::RParen && self.peek() != &Token::Eof {
                match self.advance() {
                    Token::Ident(p) => params.push(p),
                    _ => {
                        is_arrow = false;
                        break;
                    }
                }
                if self.peek() == &Token::Comma {
                    self.advance();
                } else if self.peek() != &Token::RParen {
                    is_arrow = false;
                    break;
                }
            }
            if is_arrow && self.peek() == &Token::RParen && self.peek_at(1) == &Token::Arrow {
                self.advance(); // )
                self.advance(); // =>
                let body = self.parse_closure_body(depth)?;
                return Ok(Expr::Closure { params, body });
            }
            // Otherwise restore and parse as parenthesized expression
            self.pos = saved_pos;
            self.advance(); // (
            let inner = self.parse_expr(depth + 1)?;
            self.expect(&Token::RParen)?;
            return Ok(inner);
        }

        match self.advance() {
            Token::Int(i) => Ok(Expr::Literal(JsonValue::from(i))),
            Token::Float(f) => Ok(Expr::Literal(JsonValue::from(f))),
            Token::Bool(b) => Ok(Expr::Literal(JsonValue::Bool(b))),
            Token::Str(s) => Ok(Expr::Literal(JsonValue::from(s))),
            Token::Template(raw_parts) => {
                let mut parsed_parts = Vec::with_capacity(raw_parts.len());
                for (lit, expr_src_opt) in raw_parts {
                    let expr_opt = if let Some(expr_src) = expr_src_opt {
                        let mut sub_lexer = Lexer::new(&expr_src);
                        let sub_tokens = sub_lexer.tokenize()?;
                        let mut sub_parser = Parser::new(sub_tokens, self.max_expr_depth);
                        Some(sub_parser.parse_expr(depth + 1)?)
                    } else {
                        None
                    };
                    parsed_parts.push((lit, expr_opt));
                }
                Ok(Expr::Template(parsed_parts))
            }
            Token::Null => Ok(Expr::Literal(JsonValue::Null)),
            Token::LBracket => {
                let mut items = Vec::new();
                while self.peek() != &Token::RBracket && self.peek() != &Token::Eof {
                    items.push(self.parse_expr(depth + 1)?);
                    if self.peek() == &Token::Comma {
                        self.advance();
                    } else {
                        break;
                    }
                }
                self.expect(&Token::RBracket)?;
                Ok(Expr::Array(items))
            }
            Token::LBrace | Token::HashBrace => {
                let mut pairs = Vec::new();
                while self.peek() != &Token::RBrace && self.peek() != &Token::Eof {
                    let key = match self.advance() {
                        Token::Ident(k) | Token::Str(k) => k,
                        other => {
                            return Err(CodeModeError::SyntaxError(format!(
                                "Expected object key, got {:?}",
                                other
                            )))
                        }
                    };
                    let val = if self.peek() == &Token::Colon {
                        self.advance();
                        self.parse_expr(depth + 1)?
                    } else {
                        // ES6 object property shorthand `{ count, total }`
                        Expr::Var(key.clone())
                    };
                    pairs.push((key, val));
                    if self.peek() == &Token::Comma {
                        self.advance();
                    } else {
                        break;
                    }
                }
                self.expect(&Token::RBrace)?;
                Ok(Expr::Object(pairs))
            }
            Token::Ident(name) => {
                // Namespace call `service::tool(...)`
                if self.peek() == &Token::DoubleColon {
                    self.advance();
                    let func = match self.advance() {
                        Token::Ident(f) => f,
                        other => {
                            return Err(CodeModeError::SyntaxError(format!(
                                "Expected function name after '::', got {:?}",
                                other
                            )))
                        }
                    };
                    let args = self.parse_arg_list(depth)?;
                    Ok(Expr::NamespaceCall {
                        namespace: Some(name),
                        func,
                        args,
                    })
                } else if self.peek() == &Token::LParen {
                    let args = self.parse_arg_list(depth)?;
                    Ok(Expr::NamespaceCall {
                        namespace: None,
                        func: name,
                        args,
                    })
                } else {
                    Ok(Expr::Var(name))
                }
            }
            other => Err(CodeModeError::SyntaxError(format!(
                "Unexpected token in expression: {:?}",
                other
            ))),
        }
    }
}
