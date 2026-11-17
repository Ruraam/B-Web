//! Abstract syntax tree for the B-Web JavaScript subset.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnaryOp {
    Negate,
    Not,
    TypeOf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
    Equals,
    NotEquals,
    StrictEquals,
    StrictNotEquals,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalOp {
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Template(Vec<Expr>),
    Identifier(String),
    Array(Vec<Expr>),
    Object(Vec<(String, Expr)>),
    Function { params: Vec<String>, body: Vec<Stmt> },
    Unary { op: UnaryOp, value: Box<Expr> },
    Binary { op: BinaryOp, left: Box<Expr>, right: Box<Expr> },
    Logical { op: LogicalOp, left: Box<Expr>, right: Box<Expr> },
    Assign { target: Box<Expr>, value: Box<Expr> },
    Conditional { test: Box<Expr>, then: Box<Expr>, otherwise: Box<Expr> },
    Member { object: Box<Expr>, property: Box<Expr> },
    Call { callee: Box<Expr>, args: Vec<Expr> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum VarKind {
    Var,
    Let,
    Const,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Empty,
    Var { kind: VarKind, name: String, value: Option<Expr> },
    Expression(Expr),
    Block(Vec<Stmt>),
    If { test: Expr, then: Box<Stmt>, otherwise: Option<Box<Stmt>> },
    While { test: Expr, body: Box<Stmt> },
    For { init: Option<Box<Stmt>>, test: Option<Expr>, update: Option<Expr>, body: Box<Stmt> },
    Function { name: String, params: Vec<String>, body: Vec<Stmt> },
    Return(Option<Expr>),
    Break,
    Continue,
}
