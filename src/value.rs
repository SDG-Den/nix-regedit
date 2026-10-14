use rnix::ast::Expr;
use rowan::ast::AstNode;

/// The shape of an option's value, used to pick a suitable editor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValueKind {
    Bool,
    Int,
    Float,
    String,
    List,
    AttrSet,
    Null,
    Function,
    Other,
}

impl ValueKind {
    pub fn label(self) -> &'static str {
        match self {
            ValueKind::Bool => "boolean",
            ValueKind::Int => "integer",
            ValueKind::Float => "float",
            ValueKind::String => "string",
            ValueKind::List => "list",
            ValueKind::AttrSet => "attribute set",
            ValueKind::Null => "null",
            ValueKind::Function => "function",
            ValueKind::Other => "expression",
        }
    }

    /// Kinds for which a concrete editor exists (everything except the
    /// catch-all).
    pub fn is_concrete(self) -> bool {
        !matches!(self, ValueKind::Other | ValueKind::Function | ValueKind::Null)
    }
}

fn parse_expr(text: &str) -> Option<Expr> {
    let root = rnix::Root::parse(text).ok().ok()?;
    let mut expr = root.expr()?;
    loop {
        expr = match expr {
            Expr::With(w) => w.body()?,
            Expr::LetIn(l) => l.body()?,
            Expr::Paren(p) => p.expr()?,
            other => return Some(other),
        };
    }
}

pub fn classify(text: &str) -> ValueKind {
    let Some(expr) = parse_expr(text) else {
        return ValueKind::Other;
    };
    let raw = expr.syntax().text().to_string();
    match expr {
        Expr::Str(_) => ValueKind::String,
        Expr::List(_) => ValueKind::List,
        Expr::AttrSet(_) => ValueKind::AttrSet,
        Expr::Lambda(_) => ValueKind::Function,
        Expr::Ident(_) => match raw.as_str() {
            "true" | "false" => ValueKind::Bool,
            "null" => ValueKind::Null,
            _ => ValueKind::Other,
        },
        Expr::Literal(_) => {
            if raw.starts_with('"') {
                ValueKind::String
            } else if raw.contains('.') || raw.contains('e') || raw.contains('E') {
                ValueKind::Float
            } else {
                ValueKind::Int
            }
        }
        Expr::UnaryOp(op) => op
            .expr()
            .map(|inner| classify(&inner.syntax().text().to_string()))
            .unwrap_or(ValueKind::Other),
        _ => ValueKind::Other,
    }
}

/// Source text of each element of a list literal.
pub fn list_items(text: &str) -> Option<Vec<String>> {
    let expr = parse_expr(text)?;
    let Expr::List(list) = expr else {
        return None;
    };
    Some(
        list.items()
            .map(|item| item.syntax().text().to_string().trim().to_string())
            .collect(),
    )
}

pub fn serialize_list(items: &[String]) -> String {
    if items.is_empty() {
        "[]".to_string()
    } else {
        format!("[ {} ]", items.join(" "))
    }
}

/// Map a NixOS option type (as rendered by nixd) to the closest editor kind.
pub fn kind_from_type_string(type_str: &str) -> ValueKind {
    let t = type_str.trim().to_lowercase();
    if t.contains("list") {
        ValueKind::List
    } else if t.contains("boolean") || t == "bool" {
        ValueKind::Bool
    } else if t.contains("floating") {
        ValueKind::Float
    } else if t.contains("integer") || t.contains("int") {
        ValueKind::Int
    } else if t.contains("attribute set") || t.contains("submodule") {
        ValueKind::AttrSet
    } else if t.contains("string") || t.contains("path") || t.contains("package") {
        ValueKind::String
    } else if t.contains("null") {
        ValueKind::Null
    } else {
        ValueKind::Other
    }
}

/// Pull the `Type:` field out of nixd's hover markdown.
pub fn extract_type(hover: &str) -> Option<String> {
    fn clean(s: &str) -> String {
        s.trim()
            .trim_matches(|c: char| c == '`' || c == '*' || c == '_' || c == ' ')
            .trim_end_matches(':')
            .trim()
            .to_string()
    }
    let mut expect_value = false;
    for line in hover.lines() {
        if expect_value {
            let value = clean(line);
            if !value.is_empty() {
                return Some(value);
            }
            continue;
        }
        let lower = line.to_lowercase();
        if !lower.contains("type") {
            continue;
        }
        if let Some(idx) = line.find(':') {
            let value = clean(&line[idx + 1..]);
            if !value.is_empty() && !value.to_lowercase().contains("type") {
                return Some(value);
            }
        }
        expect_value = true;
    }
    // nixd 2.x renders the type as the first line, then a blank line and the
    // description, e.g. `listOf (list of package)\n\nThe set of packages...`.
    for line in hover.lines() {
        let trimmed = clean(line);
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            return None;
        }
        return Some(trimmed);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_literals() {
        assert_eq!(classify("true"), ValueKind::Bool);
        assert_eq!(classify("false"), ValueKind::Bool);
        assert_eq!(classify("42"), ValueKind::Int);
        assert_eq!(classify("-3"), ValueKind::Int);
        assert_eq!(classify("1.5"), ValueKind::Float);
        assert_eq!(classify("\"hi\""), ValueKind::String);
        assert_eq!(classify("[ a b c ]"), ValueKind::List);
        assert_eq!(classify("{ a = 1; }"), ValueKind::AttrSet);
        assert_eq!(classify("null"), ValueKind::Null);
        assert_eq!(classify("{ x, ... }: x"), ValueKind::Function);
        assert_eq!(classify("with pkgs; [ a ]"), ValueKind::List);
    }

    #[test]
    fn list_round_trip() {
        let items = list_items("[ pkgs.wget \"x\" 1 ]").unwrap();
        assert_eq!(items, vec!["pkgs.wget", "\"x\"", "1"]);
        assert_eq!(serialize_list(&items), "[ pkgs.wget \"x\" 1 ]");
        assert_eq!(serialize_list(&[]), "[]");
    }

    #[test]
    fn parses_nixd_type() {
        assert_eq!(
            extract_type("### foo\n\n_Type:_\nboolean\n"),
            Some("boolean".to_string())
        );
        assert_eq!(
            extract_type("**Type:** list of string\n"),
            Some("list of string".to_string())
        );
        assert_eq!(kind_from_type_string("list of string"), ValueKind::List);
        assert_eq!(kind_from_type_string("boolean"), ValueKind::Bool);
        assert_eq!(kind_from_type_string("signed integer"), ValueKind::Int);
    }

    #[test]
    fn parses_nixd_2x_hover() {
        let hover = "listOf (list of package)\n\nThe set of packages that appear in /run/current-system/sw.";
        assert_eq!(
            extract_type(hover),
            Some("listOf (list of package)".to_string())
        );
        assert_eq!(kind_from_type_string("listOf (list of package)"), ValueKind::List);
        let string_hover = "strMatching \"^\\$\" (string matching the pattern)\n\nDocs";
        assert_eq!(kind_from_type_string(&extract_type(string_hover).unwrap()), ValueKind::String);
    }
}
