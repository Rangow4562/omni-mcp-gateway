use super::*;

// ============================================================================
// Zero-Copy Columnar RecordBatch & Vectorized PhysicalExpr Engine
// ============================================================================

#[derive(Clone, Debug)]
pub(super) enum ColumnVector {
    Float64(Vec<f64>, Vec<bool>),
    Utf8(Vec<String>, Vec<bool>),
    Bool(Vec<bool>, Vec<bool>),
}

pub(super) struct ColumnarRecordBatch<'a> {
    rows: &'a [JsonValue],
    num_rows: usize,
    columns: BTreeMap<String, ColumnVector>,
}

impl<'a> ColumnarRecordBatch<'a> {
    pub(super) fn from_rows(
        rows: &'a [JsonValue],
        referenced_cols: &BTreeSet<String>,
    ) -> Option<Self> {
        if rows.is_empty() {
            return None;
        }
        let num_rows = rows.len();
        let mut columns = BTreeMap::new();

        for col_name in referenced_cols {
            // Inspect first non-null value in this column to determine physical column storage type
            let mut first_val: Option<&JsonValue> = None;
            for r in rows {
                if let Some(v) = Self::extract_field(r, col_name) {
                    if !v.is_null() {
                        first_val = Some(v);
                        break;
                    }
                }
            }

            let col_vec = match first_val {
                Some(JsonValue::Int(_)) | Some(JsonValue::Float(_)) => {
                    let mut vals = Vec::with_capacity(num_rows);
                    let mut valid = Vec::with_capacity(num_rows);
                    for r in rows {
                        if let Some(v) = Self::extract_field(r, col_name).and_then(|x| x.as_f64()) {
                            vals.push(v);
                            valid.push(true);
                        } else {
                            vals.push(0.0);
                            valid.push(false);
                        }
                    }
                    ColumnVector::Float64(vals, valid)
                }
                Some(JsonValue::Bool(_)) => {
                    let mut vals = Vec::with_capacity(num_rows);
                    let mut valid = Vec::with_capacity(num_rows);
                    for r in rows {
                        if let Some(v) = Self::extract_field(r, col_name).and_then(|x| x.as_bool())
                        {
                            vals.push(v);
                            valid.push(true);
                        } else {
                            vals.push(false);
                            valid.push(false);
                        }
                    }
                    ColumnVector::Bool(vals, valid)
                }
                _ => {
                    let mut vals = Vec::with_capacity(num_rows);
                    let mut valid = Vec::with_capacity(num_rows);
                    for r in rows {
                        if let Some(v) = Self::extract_field(r, col_name).and_then(|x| x.as_str()) {
                            vals.push(v.to_string());
                            valid.push(true);
                        } else {
                            vals.push(String::new());
                            valid.push(false);
                        }
                    }
                    ColumnVector::Utf8(vals, valid)
                }
            };
            columns.insert(col_name.clone(), col_vec);
        }

        Some(Self {
            rows,
            num_rows,
            columns,
        })
    }

    #[inline]
    pub(super) fn extract_field<'r>(row: &'r JsonValue, col: &str) -> Option<&'r JsonValue> {
        row.get(col)
            .or_else(|| row.get("data").and_then(|d| d.get(col)))
    }
}

#[derive(Clone, Debug)]
pub(super) enum PhysicalExpr {
    Column(String),
    LiteralNum(f64),
    LiteralStr(String),
    LiteralBool(bool),
    Compare {
        col: String,
        op: BinOp,
        rhs_num: Option<f64>,
        rhs_str: Option<String>,
        rhs_bool: Option<bool>,
    },
    And(Box<PhysicalExpr>, Box<PhysicalExpr>),
    Or(Box<PhysicalExpr>, Box<PhysicalExpr>),
    Not(Box<PhysicalExpr>),
    Arithmetic {
        left: Box<PhysicalExpr>,
        op: BinOp,
        right: Box<PhysicalExpr>,
    },
    StructProjection(Vec<(String, PhysicalExpr)>),
}

impl PhysicalExpr {
    pub(super) fn try_compile_closure(
        closure_expr: &Expr,
        env: &BTreeMap<String, JsonValue>,
    ) -> Option<(Self, BTreeSet<String>)> {
        let (params, body) = match closure_expr {
            Expr::Closure { params, body } if params.len() == 1 => (params, body),
            _ => return None,
        };
        let row_param = &params[0];
        let body_expr = match body {
            ClosureBody::Expr(e) => e.as_ref(),
            ClosureBody::Block(stmts) if stmts.len() == 1 => match &stmts[0] {
                Stmt::Return(Some(e)) | Stmt::Expr(e) => e,
                _ => return None,
            },
            _ => return None,
        };

        let mut cols = BTreeSet::new();
        let phys = Self::compile_expr(body_expr, row_param, env, &mut cols)?;
        Some((phys, cols))
    }

    pub(super) fn compile_expr(
        expr: &Expr,
        row_param: &str,
        env: &BTreeMap<String, JsonValue>,
        cols: &mut BTreeSet<String>,
    ) -> Option<Self> {
        match expr {
            Expr::Field(target, col_name) => {
                if let Expr::Var(v) = target.as_ref() {
                    if v == row_param {
                        cols.insert(col_name.clone());
                        return Some(PhysicalExpr::Column(col_name.clone()));
                    }
                }
                None
            }
            Expr::Literal(JsonValue::Int(i)) => Some(PhysicalExpr::LiteralNum(*i as f64)),
            Expr::Literal(JsonValue::Float(f)) => Some(PhysicalExpr::LiteralNum(*f)),
            Expr::Literal(JsonValue::Str(s)) => Some(PhysicalExpr::LiteralStr(s.clone())),
            Expr::Literal(JsonValue::Bool(b)) => Some(PhysicalExpr::LiteralBool(*b)),
            Expr::Var(name) if name != row_param => match env.get(name)? {
                JsonValue::Int(i) => Some(PhysicalExpr::LiteralNum(*i as f64)),
                JsonValue::Float(f) => Some(PhysicalExpr::LiteralNum(*f)),
                JsonValue::Str(s) => Some(PhysicalExpr::LiteralStr(s.clone())),
                JsonValue::Bool(b) => Some(PhysicalExpr::LiteralBool(*b)),
                _ => None,
            },
            Expr::Unary(UnaryOp::Not, inner) => {
                let p = Self::compile_expr(inner, row_param, env, cols)?;
                Some(PhysicalExpr::Not(Box::new(p)))
            }
            Expr::Binary(left, op, right) => {
                if matches!(op, BinOp::And) {
                    let l = Self::compile_expr(left, row_param, env, cols)?;
                    let r = Self::compile_expr(right, row_param, env, cols)?;
                    return Some(PhysicalExpr::And(Box::new(l), Box::new(r)));
                }
                if matches!(op, BinOp::Or) {
                    let l = Self::compile_expr(left, row_param, env, cols)?;
                    let r = Self::compile_expr(right, row_param, env, cols)?;
                    return Some(PhysicalExpr::Or(Box::new(l), Box::new(r)));
                }
                if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div) {
                    let l = Self::compile_expr(left, row_param, env, cols)?;
                    let r = Self::compile_expr(right, row_param, env, cols)?;
                    return Some(PhysicalExpr::Arithmetic {
                        left: Box::new(l),
                        op: *op,
                        right: Box::new(r),
                    });
                }
                if matches!(
                    op,
                    BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq
                ) {
                    let l_phys = Self::compile_expr(left, row_param, env, cols)?;
                    let r_phys = Self::compile_expr(right, row_param, env, cols)?;
                    if let PhysicalExpr::Column(col) = l_phys {
                        match r_phys {
                            PhysicalExpr::LiteralNum(n) => {
                                return Some(PhysicalExpr::Compare {
                                    col,
                                    op: *op,
                                    rhs_num: Some(n),
                                    rhs_str: None,
                                    rhs_bool: None,
                                })
                            }
                            PhysicalExpr::LiteralStr(s) => {
                                return Some(PhysicalExpr::Compare {
                                    col,
                                    op: *op,
                                    rhs_num: None,
                                    rhs_str: Some(s),
                                    rhs_bool: None,
                                })
                            }
                            PhysicalExpr::LiteralBool(b) => {
                                return Some(PhysicalExpr::Compare {
                                    col,
                                    op: *op,
                                    rhs_num: None,
                                    rhs_str: None,
                                    rhs_bool: Some(b),
                                })
                            }
                            _ => return None,
                        }
                    }
                }
                None
            }
            Expr::Object(pairs) => {
                let mut proj = Vec::with_capacity(pairs.len());
                for (k, v_expr) in pairs {
                    let p = Self::compile_expr(v_expr, row_param, env, cols)?;
                    proj.push((k.clone(), p));
                }
                Some(PhysicalExpr::StructProjection(proj))
            }
            _ => None,
        }
    }

    pub(super) fn eval_filter_mask(&self, batch: &ColumnarRecordBatch<'_>) -> Option<Vec<bool>> {
        let n = batch.num_rows;
        match self {
            PhysicalExpr::Column(col) => match batch.columns.get(col)? {
                ColumnVector::Bool(vals, valid) => {
                    Some((0..n).map(|i| valid[i] && vals[i]).collect())
                }
                ColumnVector::Float64(vals, valid) => {
                    Some((0..n).map(|i| valid[i] && vals[i] != 0.0).collect())
                }
                ColumnVector::Utf8(vals, valid) => {
                    Some((0..n).map(|i| valid[i] && !vals[i].is_empty()).collect())
                }
            },
            PhysicalExpr::Compare {
                col,
                op,
                rhs_num,
                rhs_str,
                rhs_bool,
            } => {
                let col_vec = batch.columns.get(col)?;
                match (col_vec, rhs_num, rhs_str, rhs_bool) {
                    (ColumnVector::Float64(vals, valid), Some(target), _, _) => {
                        let t = *target;
                        let mask: Vec<bool> = match op {
                            BinOp::Eq => (0..n)
                                .map(|i| valid[i] && (vals[i] - t).abs() < 1e-9)
                                .collect(),
                            BinOp::NotEq => (0..n)
                                .map(|i| !valid[i] || (vals[i] - t).abs() >= 1e-9)
                                .collect(),
                            BinOp::Lt => (0..n).map(|i| valid[i] && vals[i] < t).collect(),
                            BinOp::LtEq => (0..n).map(|i| valid[i] && vals[i] <= t).collect(),
                            BinOp::Gt => (0..n).map(|i| valid[i] && vals[i] > t).collect(),
                            BinOp::GtEq => (0..n).map(|i| valid[i] && vals[i] >= t).collect(),
                            _ => return None,
                        };
                        Some(mask)
                    }
                    (ColumnVector::Utf8(vals, valid), _, Some(target), _) => {
                        let mask: Vec<bool> = match op {
                            BinOp::Eq => (0..n).map(|i| valid[i] && &vals[i] == target).collect(),
                            BinOp::NotEq => {
                                (0..n).map(|i| !valid[i] || &vals[i] != target).collect()
                            }
                            BinOp::Lt => (0..n).map(|i| valid[i] && &vals[i] < target).collect(),
                            BinOp::LtEq => (0..n).map(|i| valid[i] && &vals[i] <= target).collect(),
                            BinOp::Gt => (0..n).map(|i| valid[i] && &vals[i] > target).collect(),
                            BinOp::GtEq => (0..n).map(|i| valid[i] && &vals[i] >= target).collect(),
                            _ => return None,
                        };
                        Some(mask)
                    }
                    (ColumnVector::Bool(vals, valid), _, _, Some(target)) => {
                        let t = *target;
                        let mask: Vec<bool> = match op {
                            BinOp::Eq => (0..n).map(|i| valid[i] && vals[i] == t).collect(),
                            BinOp::NotEq => (0..n).map(|i| !valid[i] || vals[i] != t).collect(),
                            _ => return None,
                        };
                        Some(mask)
                    }
                    _ => None,
                }
            }
            PhysicalExpr::And(left, right) => {
                let l_mask = left.eval_filter_mask(batch)?;
                let r_mask = right.eval_filter_mask(batch)?;
                Some((0..n).map(|i| l_mask[i] && r_mask[i]).collect())
            }
            PhysicalExpr::Or(left, right) => {
                let l_mask = left.eval_filter_mask(batch)?;
                let r_mask = right.eval_filter_mask(batch)?;
                Some((0..n).map(|i| l_mask[i] || r_mask[i]).collect())
            }
            PhysicalExpr::Not(inner) => {
                let m = inner.eval_filter_mask(batch)?;
                Some(m.into_iter().map(|b| !b).collect())
            }
            _ => None,
        }
    }

    pub(super) fn eval_scalar_at(
        &self,
        batch: &ColumnarRecordBatch<'_>,
        row_idx: usize,
    ) -> Option<JsonValue> {
        match self {
            PhysicalExpr::Column(col) => {
                ColumnarRecordBatch::extract_field(&batch.rows[row_idx], col).cloned()
            }
            PhysicalExpr::LiteralNum(n) => {
                if n.fract() == 0.0 {
                    Some(JsonValue::from(*n as i64))
                } else {
                    Some(JsonValue::from(*n))
                }
            }
            PhysicalExpr::LiteralStr(s) => Some(JsonValue::from(s.clone())),
            PhysicalExpr::LiteralBool(b) => Some(JsonValue::Bool(*b)),
            PhysicalExpr::Arithmetic { left, op, right } => {
                let lv = left.eval_scalar_at(batch, row_idx)?;
                let rv = right.eval_scalar_at(batch, row_idx)?;
                VmState::eval_bin_op(&lv, *op, &rv).ok()
            }
            PhysicalExpr::StructProjection(fields) => {
                let mut obj = JsonValue::object();
                for (k, f_expr) in fields {
                    let val = f_expr
                        .eval_scalar_at(batch, row_idx)
                        .unwrap_or(JsonValue::Null);
                    obj.insert(k.clone(), val);
                }
                Some(obj)
            }
            _ => None,
        }
    }

    pub(super) fn eval_projection_columnar(
        &self,
        batch: &ColumnarRecordBatch<'_>,
    ) -> Option<Vec<JsonValue>> {
        let mut out = Vec::with_capacity(batch.num_rows);
        for i in 0..batch.num_rows {
            out.push(self.eval_scalar_at(batch, i)?);
        }
        Some(out)
    }
}
