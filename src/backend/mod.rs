//! Backend layer: type system, IR emission, module resolution, and build pipeline.

pub mod types;
pub mod ir_emitter;
pub mod module_resolver;
pub mod link_table;
pub mod driver;
