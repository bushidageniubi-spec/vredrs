use vredrs_compiler::{bytecode::Compiler, lexer::Lexer, parser::Parser};

fn main() {
    let source = "set, f, fn(x) x + 1\nprintln, f(10)\n";
    let mut lexer = Lexer::new(source, 0);
    let tokens = lexer.tokenize().unwrap();
    let mut parser = Parser::new(tokens, 0);
    let program = parser.parse_program().unwrap();
    
    let compiler = Compiler::new();
    let module = compiler.compile(&program).unwrap();
    
    eprintln!("=== Module functions ===");
    for (name, fd) in &module.functions {
        eprintln!("  {} params={:?}", name, fd.params);
    }
    eprintln!("=== fn_entry_pcs ===");
    for (name, (pc, nlocals)) in &module.fn_entry_pcs {
        eprintln!("  {} entry_pc={} nlocals={}", name, pc, nlocals);
    }
    eprintln!("=== Constants ===");
    for (i, c) in module.constants.iter().enumerate() {
        eprintln!("  [{}] = {:?}", i, c);
    }
    eprintln!("=== Code ===");
    for (i, instr) in module.code.iter().enumerate() {
        eprintln!("  {:3} {:?}", i, instr);
        if i > 50 { break; }
    }
    
    let mut vm = vredrs_compiler::bytecode::VM::new(module).with_program(program);
    match vm.run() {
        Ok(v) => eprintln!("Result: {:?}", v),
        Err(e) => eprintln!("Error: {}", e.message()),
    }
}
