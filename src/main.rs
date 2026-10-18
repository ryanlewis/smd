const VERSION: &str = env!("CARGO_PKG_VERSION");

fn version_line() -> String {
    format!("smd {VERSION}")
}

fn main() {
    if std::env::args()
        .skip(1)
        .any(|a| a == "--version" || a == "-V")
    {
        println!("{}", version_line());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_has_name_and_version() {
        assert_eq!(version_line(), format!("smd {}", env!("CARGO_PKG_VERSION")));
    }
}
