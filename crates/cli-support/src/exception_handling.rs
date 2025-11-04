use anyhow::Result;
use std::collections::HashMap;
use walrus::ir::*;
use walrus::{FunctionId, InstrSeqBuilder, Module, RefType, TableId, TagId, TypeId, ValType};

struct Transform {
    // A map of old import functions to the new internally-defined shims which
    // call the correct new import functions
    import_map: HashMap<FunctionId, TypeId>,
    jstag: TagId,
    jspanic: FunctionId,
}

#[derive(Default)]
struct FindCalls {
    calls: Vec<(InstrSeqId, usize, FunctionId, bool)>,
}

impl FindCalls {
    fn new() -> Self {
        return Default::default();
    }
}

impl Visitor<'_> for FindCalls {
    fn start_instr_seq(&mut self, seq: &InstrSeq) {
        for i in (0..seq.instrs.len()).rev() {
            let (func, return_call) = match &seq.instrs[i].0 {
                Instr::Call(Call { func }) => (func, false),
                Instr::ReturnCall(ReturnCall { func }) => (func, true),
                _ => continue,
            };
            self.calls.push((seq.id(), i, *func, return_call));
        }
    }
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
            let func_id = match import.kind {
                walrus::ImportKind::Function(f) => f,
                _ => continue,
            };
            if import.name.starts_with("__wbg_") && !import.name.starts_with("__wbg___") {
                println!("Will add exception handling to import: {}", import.name);
                let func = module.funcs.get(func_id);
                self.import_map.insert(func_id, func.ty());
            }
        }
        Ok(())
    }

    fn set_block_loc(seq_builder: &mut InstrSeqBuilder, loc: InstrLocId) {
        for (_, loc1) in seq_builder.instrs_mut() {
            *loc1 = loc;
        }
    }

    fn transform_calls(&self, module: &mut Module) -> Result<()> {
        for (_id, func) in module.funcs.iter_local_mut() {
            let mut visitor = FindCalls::new();
            let entry = func.entry_block();
            dfs_in_order(&mut visitor, func, entry);
            let func_builder = func.builder_mut();
            for (instr_seq_id, i, target_id, return_call) in visitor.calls {
                let Some(ty) = self.import_map.get(&target_id) else {
                    continue;
                };
                let mut instr_builder = func_builder.instr_seq(instr_seq_id);
                let loc = instr_builder.instrs()[i].1.clone();

                // Try block
                let mut try_block_builder =
                    instr_builder.dangling_instr_seq(InstrSeqType::MultiValue(*ty));
                if return_call {
                    try_block_builder.return_call(target_id);
                } else {
                    try_block_builder.call(target_id);
                }
                Self::set_block_loc(&mut try_block_builder, loc);
                let try_block = try_block_builder.id();

                // Catch block
                let mut catch_block_builder = instr_builder.dangling_instr_seq(None);
                catch_block_builder.call(self.jspanic);
                catch_block_builder.unreachable();
                Self::set_block_loc(&mut catch_block_builder, loc);
                let catch_block = catch_block_builder.id();

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
    }

    fn make_js_panic(module: &mut Module) -> Result<FunctionId> {
        let js_panic_helper = module.funcs.by_name("___wbg_js_panic").unwrap();
        let table_alloc = module.funcs.by_name("__externref_table_alloc").unwrap();
        let mut xref_table: Option<TableId> = None;
        for table in module.tables.iter() {
            if table.name.as_ref().map(|x| &**x) != Some("__wbindgen_externrefs") {
                continue;
            }
            xref_table = Some(table.id());
        }
        let mut builder = walrus::FunctionBuilder::new(
            &mut module.types,
            &[ValType::Ref(RefType::Externref)],
            &[],
        );
        builder.name("js_panic".into());
        let arg = module.locals.add(ValType::Ref(RefType::Externref));
        let scratch = module.locals.add(ValType::I32);
        let mut body = builder.func_body();
        body.call(table_alloc);
        body.local_tee(scratch);
        body.local_get(arg);
        body.table_set(xref_table.unwrap());
        body.local_get(scratch);
        body.call(js_panic_helper);
        let result = builder.finish(vec![arg], &mut module.funcs);
        Ok(result)
    }

    fn run(module: &mut Module, import_name: &str) -> Result<()> {
        let ty = module
            .types
            .find(&[], &[ValType::Ref(RefType::Externref)])
            .unwrap_or_else(|| module.types.add(&[ValType::Ref(RefType::Externref)], &[]));
        let (jstag, _) = module.add_import_tag(import_name, "JSTag", ty);
        let js_panic = Self::make_js_panic(module)?;
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
