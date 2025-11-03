use anyhow::Result;
use std::collections::HashMap;
use walrus::Module;
use walrus::{ir::*, RefType};
use walrus::{FunctionId, TagId, TypeId, ValType};

struct Transform {
    // A map of old import functions to the new internally-defined shims which
    // call the correct new import functions
    import_map: HashMap<FunctionId, TypeId>,
    jstag: TagId,
    jspanic: FunctionId,
}

impl Transform {
    fn new(jstag: TagId, jspanic: FunctionId) -> Self {
        Self {
            import_map: HashMap::new(),
            jstag,
            jspanic,
        }
    }
    fn process_imports(&mut self, module: &mut Module) -> Result<()> {
        for import in module.imports.iter_mut() {
            let f = match import.kind {
                walrus::ImportKind::Function(f) => f,
                _ => continue,
            };
            if import.name.starts_with("__wbg_") && !import.name.starts_with("__wbg___") {
                println!("import.name: {}", import.name);
                let f2 = module.funcs.get(f);
                self.import_map.insert(f, f2.ty());
            }
        }
        Ok(())
    }

    fn transform_calls(&self, module: &mut Module) -> Result<()> {
        for (_id, func) in module.funcs.iter_local_mut() {
            let mut visitor = Rewrite { targets: vec![] };
            let entry = func.entry_block();
            dfs_in_order(&mut visitor, func, entry);
            let func_builder = func.builder_mut();
            for (instr_seq_id, i, target_id, return_call) in visitor.targets {
                let Some(ty) = self.import_map.get(&target_id) else {
                    continue;
                };
                let mut instr_builder = func_builder.instr_seq(instr_seq_id);
                let loc = instr_builder.instrs()[i].1.clone();
                let mut try_block_builder =
                    instr_builder.dangling_instr_seq(InstrSeqType::MultiValue(*ty));
                if return_call {
                    try_block_builder.return_call(target_id);
                } else {
                    try_block_builder.call(target_id);
                }
                for (_, loc1) in try_block_builder.instrs_mut() {
                    *loc1 = loc;
                }
                let try_block = try_block_builder.id();
                drop(try_block_builder);
                let mut catch_block_builder = instr_builder.dangling_instr_seq(None);
                catch_block_builder.call(self.jspanic);
                catch_block_builder.unreachable();
                let catch_block = catch_block_builder.id();
                for (_, loc1) in catch_block_builder.instrs_mut() {
                    *loc1 = loc;
                }
                instr_builder.instrs_mut()[i].0 = Try {
                    seq: try_block,
                    catches: vec![LegacyCatch::Catch {
                        tag: self.jstag,
                        handler: catch_block,
                    }],
                }
                .into();
            }
        }

        return Ok(());

        struct Rewrite {
            targets: Vec<(InstrSeqId, usize, FunctionId, bool)>,
        }

        impl Visitor<'_> for Rewrite {
            fn start_instr_seq(&mut self, seq: &InstrSeq) {
                for i in (0..seq.instrs.len()).rev() {
                    let (func, return_call) = match &seq.instrs[i].0 {
                        Instr::Call(Call { func }) => (func, false),
                        Instr::ReturnCall(ReturnCall { func }) => (func, true),
                        _ => continue,
                    };
                    self.targets.push((seq.id(), i, *func, return_call));
                }
            }
        }
    }

    fn run(module: &mut Module, import_name: &str) -> Result<()> {
        let ty = module
            .types
            .find(&[], &[ValType::Ref(RefType::Externref)])
            .unwrap_or_else(|| module.types.add(&[ValType::Ref(RefType::Externref)], &[]));
        let (jstag, _) = module.add_import_tag(import_name, "JSTag", ty);
        let js_panic = module.funcs.by_name("___wbg_js_panic").unwrap();

        let mut transform = Transform::new(jstag, js_panic);

        transform.process_imports(module)?;
        transform.transform_calls(module)?;
        Ok(())
    }
}

pub fn process(module: &mut Module, import_name: &str) -> Result<()> {
    Transform::run(module, import_name)?;

    Ok(())
}
