#[cfg(test)]
mod tests {
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::semantic::resolver::Resolver;
    use crate::semantic::type_checker::TypeChecker;

    fn resolve_and_check(source: &str) -> Result<(), String> {
        let mut lexer = Lexer::new(source, 0);
        let tokens = lexer.tokenize().map_err(|e| format!("Lex error: {}", e))?;
        let mut parser = Parser::new(tokens, 0);
        let program = parser.parse_program().map_err(|e| format!("Parse error: {}", e))?;
        let resolver = Resolver::new();
        let symtab = resolver.resolve(&program).map_err(|e| format!("Resolve error: {}", e))?;
        let type_checker = TypeChecker::new(symtab);
        type_checker.check(&program).map_err(|e| format!("Type error: {}", e))?;
        Ok(())
    }

    #[test]
    fn test_simple_assignment() {
        assert!(resolve_and_check("set, x, 42").is_ok());
    }

    #[test]
    fn test_fn_def_resolves() {
        assert!(resolve_and_check("fn, add(a, b)\n    return, a + b\n/end").is_ok());
    }

    #[test]
    fn test_undeclared_variable() {
        // In vredrs 1.0 dynamic mode, undefined variable access is a runtime error, not compile-time
        assert!(resolve_and_check("set, y, x").is_ok());
    }

    #[test]
    fn test_if_typecheck() {
        assert!(resolve_and_check("if, true then paste, ok").is_ok());
    }

    #[test]
    fn test_for_in_resolves() {
        assert!(resolve_and_check("for, i, in, 1..10\n    println, i\n/end").is_ok());
    }

    #[test]
    fn test_struct_declaration() {
        assert!(resolve_and_check("struct, Point\n    x, int\n    y, int\n/end").is_ok());
    }

    #[test]
    fn test_class_resolves() {
        assert!(resolve_and_check("class, Animal\n    private, name\n    fn, new(n)\n        self.name = n\n    /end\n/end").is_ok());
    }

    #[test]
    fn test_enum_resolves() {
        assert!(resolve_and_check("enum, Color\n    Red\n    Green\n    Blue\n/end").is_ok());
    }

    #[test]
    fn test_type_alias_resolves() {
        assert!(resolve_and_check("type, MyInt = int").is_ok());
    }

    #[test]
    fn test_binary_ops_typecheck() {
        assert!(resolve_and_check("set, x, 1 + 2 * 3").is_ok());
        assert!(resolve_and_check("set, flag, a > b and c <= d").is_ok());
    }

    #[test]
    fn test_ternary_typecheck() {
        assert!(resolve_and_check("set, x, a > b ? 1 : 0").is_ok());
    }

    #[test]
    fn test_list_typecheck() {
        assert!(resolve_and_check("set, lst, [1, 2, 3]").is_ok());
    }

    #[test]
    fn test_dict_typecheck() {
        assert!(resolve_and_check("set, d, {x: 1, y: 2}").is_ok());
    }

    #[test]
    fn test_lambda_resolves() {
        assert!(resolve_and_check("set, square, fn(x) x * x").is_ok());
    }

    #[test]
    fn test_spawn_resolves() {
        assert!(resolve_and_check("spawn, fn() println, hello").is_ok());
    }

    #[test]
    fn test_match_resolves() {
        let src = "match, x\n    case, 0\n        println, zero\n    case, n if n > 0\n        println, positive\n/end";
        assert!(resolve_and_check(src).is_ok());
    }

    #[test]
    fn test_try_resolves() {
        let src = "try\n    risky()\ncatch, e\n    println, e\n/end";
        assert!(resolve_and_check(src).is_ok());
    }

    #[test]
    fn test_constexpr_resolves() {
        assert!(resolve_and_check("constexpr, PI = 3.14159").is_ok());
    }

    #[test]
    fn test_lazy_resolves() {
        assert!(resolve_and_check("lazy, set, data = read(big.txt)").is_ok());
    }
}




