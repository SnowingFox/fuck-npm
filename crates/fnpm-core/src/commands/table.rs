//! bun's box tables (`bun_core::fmt::Table`): one space of padding on each side of a cell,
//! box-drawing characters on a colour terminal and `|`/`-` otherwise.

/// The symbols of one table style.
struct Symbols {
    top: [&'static str; 3],
    middle: [&'static str; 3],
    bottom: [&'static str; 3],
    horizontal: &'static str,
    vertical: &'static str,
}

const UNICODE: Symbols = Symbols {
    top: ["┌", "┬", "┐"],
    middle: ["├", "┼", "┤"],
    bottom: ["└", "┴", "┘"],
    horizontal: "─",
    vertical: "│",
};

const ASCII: Symbols = Symbols {
    top: ["|", "-", "|"],
    middle: ["|", "|", "|"],
    bottom: ["|", "-", "|"],
    horizontal: "-",
    vertical: "|",
};

fn width(s: &str) -> usize {
    s.chars().count()
}

/// Renders `header` and `rows` with a separator line between rows (bun prints one before
/// every row).
pub(crate) fn render(header: &[&str], rows: &[Vec<String>], unicode: bool) -> Vec<String> {
    let sym = if unicode { &UNICODE } else { &ASCII };
    let mut widths: Vec<usize> = header.iter().map(|h| width(h)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(widths.len()) {
            widths[i] = widths[i].max(width(cell));
        }
    }
    let line = |edges: &[&str; 3]| {
        let mut s = String::from(edges[0]);
        for (i, w) in widths.iter().enumerate() {
            if i > 0 {
                s.push_str(edges[1]);
            }
            s.push_str(&sym.horizontal.repeat(w + 2));
        }
        s.push_str(edges[2]);
        s
    };
    let cells = |cells: &[String]| {
        let mut s = String::from(sym.vertical);
        for (i, w) in widths.iter().enumerate() {
            let c = cells.get(i).map(String::as_str).unwrap_or("");
            s.push(' ');
            s.push_str(c);
            s.push_str(&" ".repeat(w - width(c) + 1));
            s.push_str(sym.vertical);
        }
        s
    };
    let mut out = vec![line(&sym.top)];
    out.push(cells(
        &header.iter().map(|h| (*h).to_owned()).collect::<Vec<_>>(),
    ));
    for row in rows {
        out.push(line(&sym.middle));
        out.push(cells(row));
    }
    out.push(line(&sym.bottom));
    out
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn ascii_and_unicode() {
        let rows = vec![vec!["a".to_owned(), "1.0.0".to_owned()]];
        assert_eq!(
            render(&["Package", "Current"], &rows, false),
            vec![
                "|-------------------|",
                "| Package | Current |",
                "|---------|---------|",
                "| a       | 1.0.0   |",
                "|-------------------|",
            ]
        );
        let u = render(&["P"], &rows, true);
        assert_eq!(u[0], "┌───┐");
        assert_eq!(u[3], "│ a │");
        assert_eq!(u[4], "└───┘");
    }
}
