use super::*;

// ============================================================================
// Static AST Auto-Parallelizer (SSA Dataflow Dependency Analyzer)
// ============================================================================

pub(super) struct AstAutoParallelizer;

impl AstAutoParallelizer {
    pub(super) fn optimize_block(stmts: Vec<Stmt>) -> Vec<Stmt> {
        let mut out = Vec::with_capacity(stmts.len());
        let mut pending_wave: Vec<ParallelTask> = Vec::new();
        let mut wave_written_vars: BTreeSet<String> = BTreeSet::new();

        let flush_wave = |wave: &mut Vec<ParallelTask>,
                          written: &mut BTreeSet<String>,
                          target: &mut Vec<Stmt>| {
            if wave.is_empty() {
                return;
            }
            if wave.len() == 1 {
                let task = wave.remove(0);
                let call_expr = Expr::MethodCall {
                    receiver: Box::new(Expr::Field(
                        Box::new(Expr::Var("codemode".to_string())),
                        task.server_name,
                    )),
                    method: task.tool_name,
                    args: task.args,
                };
                if let Some(var_name) = task.target_var {
                    target.push(Stmt::Let {
                        name: var_name,
                        expr: call_expr,
                    });
                } else {
                    target.push(Stmt::Expr(call_expr));
                }
            } else {
                target.push(Stmt::ParallelWave(std::mem::take(wave)));
            }
            written.clear();
        };

        for stmt in stmts {
            if let Some(task) = Self::extract_parallelizable_task(&stmt) {
                let mut read_vars = BTreeSet::new();
                for arg in &task.args {
                    Self::collect_read_vars(arg, &mut read_vars);
                }

                // A tool call can join the current parallel wave iff its inputs do NOT depend on
                // any variable written earlier in the current wave, and its target variable doesn't collide.
                let has_data_hazard = read_vars.iter().any(|rv| wave_written_vars.contains(rv))
                    || task
                        .target_var
                        .as_ref()
                        .is_some_and(|tv| wave_written_vars.contains(tv));

                if has_data_hazard {
                    flush_wave(&mut pending_wave, &mut wave_written_vars, &mut out);
                }

                if let Some(ref tv) = task.target_var {
                    wave_written_vars.insert(tv.clone());
                }
                pending_wave.push(task);
                continue;
            }

            flush_wave(&mut pending_wave, &mut wave_written_vars, &mut out);

            let optimized_stmt = match stmt {
                Stmt::If {
                    cond,
                    then_branch,
                    else_branch,
                } => Stmt::If {
                    cond,
                    then_branch: Self::optimize_block(then_branch),
                    else_branch: else_branch.map(Self::optimize_block),
                },
                Stmt::For {
                    var,
                    iterable,
                    body,
                } => Stmt::For {
                    var,
                    iterable,
                    body: Self::optimize_block(body),
                },
                Stmt::While { cond, body } => Stmt::While {
                    cond,
                    body: Self::optimize_block(body),
                },
                other => other,
            };
            out.push(optimized_stmt);
        }

        flush_wave(&mut pending_wave, &mut wave_written_vars, &mut out);
        out
    }

    pub(super) fn extract_parallelizable_task(stmt: &Stmt) -> Option<ParallelTask> {
        match stmt {
            Stmt::Let { name, expr } => {
                let (srv, tool, args) = Self::extract_direct_tool_call(expr)?;
                Some(ParallelTask {
                    target_var: Some(name.clone()),
                    server_name: srv,
                    tool_name: tool,
                    args,
                })
            }
            Stmt::Expr(expr) => {
                let (srv, tool, args) = Self::extract_direct_tool_call(expr)?;
                Some(ParallelTask {
                    target_var: None,
                    server_name: srv,
                    tool_name: tool,
                    args,
                })
            }
            _ => None,
        }
    }

    pub(super) fn extract_direct_tool_call(expr: &Expr) -> Option<(String, String, Vec<Expr>)> {
        match expr {
            Expr::MethodCall {
                receiver,
                method,
                args,
            } => {
                if let Expr::Field(inner_obj, server_name) = receiver.as_ref() {
                    if let Expr::Var(ns) = inner_obj.as_ref() {
                        if ns == "codemode" {
                            // Do not coalesce nested expressions that themselves contain tool calls
                            if args.iter().all(|a| !Self::contains_tool_call(a)) {
                                return Some((server_name.clone(), method.clone(), args.clone()));
                            }
                        }
                    }
                }
                None
            }
            Expr::NamespaceCall {
                namespace: Some(srv),
                func,
                args,
            } => {
                if !matches!(
                    srv.as_str(),
                    "mcp" | "std" | "fs" | "net" | "process" | "http" | "https"
                ) && args.iter().all(|a| !Self::contains_tool_call(a))
                {
                    return Some((srv.clone(), func.clone(), args.clone()));
                }
                None
            }
            _ => None,
        }
    }

    pub(super) fn contains_tool_call(expr: &Expr) -> bool {
        match expr {
            Expr::NamespaceCall { .. } => true,
            Expr::MethodCall { receiver, args, .. } => {
                if let Expr::Field(inner, _) = receiver.as_ref() {
                    if let Expr::Var(ns) = inner.as_ref() {
                        if ns == "codemode" {
                            return true;
                        }
                    }
                }
                Self::contains_tool_call(receiver) || args.iter().any(Self::contains_tool_call)
            }
            Expr::Array(items) => items.iter().any(Self::contains_tool_call),
            Expr::Object(pairs) => pairs.iter().any(|(_, v)| Self::contains_tool_call(v)),
            Expr::Field(inner, _) | Expr::Unary(_, inner) => Self::contains_tool_call(inner),
            Expr::Index(a, b) | Expr::Binary(a, _, b) => {
                Self::contains_tool_call(a) || Self::contains_tool_call(b)
            }
            Expr::Ternary(a, b, c) => {
                Self::contains_tool_call(a)
                    || Self::contains_tool_call(b)
                    || Self::contains_tool_call(c)
            }
            Expr::Template(parts) => parts
                .iter()
                .any(|(_, e)| e.as_ref().map(Self::contains_tool_call).unwrap_or(false)),
            Expr::Literal(_) | Expr::Var(_) | Expr::Closure { .. } => false,
        }
    }

    pub(super) fn collect_read_vars(expr: &Expr, out: &mut BTreeSet<String>) {
        match expr {
            Expr::Var(name) => {
                if name != "codemode" && name != "console" && name != "Object" && name != "JSON" {
                    out.insert(name.clone());
                }
            }
            Expr::Array(items) => {
                for i in items {
                    Self::collect_read_vars(i, out);
                }
            }
            Expr::Object(pairs) => {
                for (_, v) in pairs {
                    Self::collect_read_vars(v, out);
                }
            }
            Expr::Field(inner, _) | Expr::Unary(_, inner) => Self::collect_read_vars(inner, out),
            Expr::Index(a, b) | Expr::Binary(a, _, b) => {
                Self::collect_read_vars(a, out);
                Self::collect_read_vars(b, out);
            }
            Expr::Ternary(a, b, c) => {
                Self::collect_read_vars(a, out);
                Self::collect_read_vars(b, out);
                Self::collect_read_vars(c, out);
            }
            Expr::NamespaceCall { args, .. } => {
                for a in args {
                    Self::collect_read_vars(a, out);
                }
            }
            Expr::MethodCall { receiver, args, .. } => {
                Self::collect_read_vars(receiver, out);
                for a in args {
                    Self::collect_read_vars(a, out);
                }
            }
            Expr::Template(parts) => {
                for (_, e) in parts {
                    if let Some(inner) = e {
                        Self::collect_read_vars(inner, out);
                    }
                }
            }
            Expr::Literal(_) | Expr::Closure { .. } => {}
        }
    }
}
