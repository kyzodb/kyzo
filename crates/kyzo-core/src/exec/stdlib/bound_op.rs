//! BoundOp: OpDecl paired with a total body. Private mint via bind_op only.
use super::errors::{DomainError, StdlibRefuse, result_has_nan};
use kyzo_model::program::op::OpDecl;
use kyzo_model::value::DataValue;
use miette::{Result, bail};

/// Builtin op with declaration + body. Fields private — construct only via [`super::bind::bind_op`].
#[derive(Clone)]
pub struct BoundOp {
    decl: OpDecl,
    body: fn(&[DataValue]) -> Result<DataValue>,
}

impl BoundOp {
    /// Sole apply door — typed NaN Refuse (`StdlibRefuse::NanAnswer`).
    pub fn apply(&self, args: &[DataValue]) -> Result<DataValue> {
        if !self.decl.arity_matches(args.len()) {
            bail!(
                "op {} expects {}, got {} argument(s)",
                self.decl.display_name(),
                self.decl.arity_requirement(),
                args.len()
            );
        }
        let result = (self.body)(args)?;
        if result_has_nan(&result) {
            bail!(StdlibRefuse::NanAnswer {
                op: self.decl.display_name().into(),
            });
        }
        Ok(result)
    }

    pub fn decl(&self) -> OpDecl {
        self.decl
    }

    pub fn name(&self) -> &'static str {
        self.decl.name
    }

    pub fn display_name(&self) -> String {
        self.decl.display_name()
    }

    pub fn min_arity(&self) -> usize {
        self.decl.min_arity
    }

    pub fn is_vararg(&self) -> bool {
        self.decl.is_vararg()
    }

    pub fn is_deterministic(&self) -> bool {
        self.decl.is_deterministic()
    }

    pub fn arity_matches(&self, n: usize) -> bool {
        self.decl.arity_matches(n)
    }

    pub fn arity_requirement(&self) -> String {
        self.decl.arity_requirement()
    }
}

impl PartialEq for BoundOp {
    fn eq(&self, other: &Self) -> bool {
        self.decl.name == other.decl.name
    }
}
impl Eq for BoundOp {}

impl std::fmt::Debug for BoundOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.decl.name)
    }
}

/// Private mint — only callable from bind.rs (same module tree; not pub).
pub(super) const fn mint(decl: OpDecl, body: fn(&[DataValue]) -> Result<DataValue>) -> BoundOp {
    BoundOp { decl, body }
}
