#!/usr/bin/env bash
# The multiplexer's demo projects (see docs/multiplexeur.md, section 4), from scratch:
#   calc/     a small Rust crate with tests (several features of one project)
#   recettes/, meteo/, agenda/   three small projects (several projects at once)
# Usage: scripts/multiplexeur-demo.sh [folder]   (default: /tmp/akolade-demo; replaced if it exists)
set -euo pipefail
DIR="${1:-/tmp/akolade-demo}"
rm -rf "$DIR"
mkdir -p "$DIR"
commit() { git -C "$1" add -A && git -C "$1" -c user.name=Demo -c user.email=demo@localhost commit -qm "$2"; }

mkdir -p "$DIR/calc/src"
git -C "$DIR/calc" init -q -b main
cat > "$DIR/calc/Cargo.toml" <<'TOML'
[package]
name = "calc"
version = "0.1.0"
edition = "2021"
TOML
cat > "$DIR/calc/src/lib.rs" <<'RUST'
/// The sum of two numbers.
pub fn add(a: i64, b: i64) -> i64 {
    a + b
}

/// The difference of two numbers.
pub fn sub(a: i64, b: i64) -> i64 {
    a - b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(2, 3), 5);
    }

    #[test]
    fn subtracts() {
        assert_eq!(sub(5, 3), 2);
    }
}
RUST
printf 'target/\n' > "$DIR/calc/.gitignore"
commit "$DIR/calc" "calc : addition et soustraction"

for p in recettes meteo agenda; do
  mkdir -p "$DIR/$p"
  git -C "$DIR/$p" init -q -b main
  printf '# %s\n\nUn petit projet de démonstration.\n' "$p" > "$DIR/$p/README.md"
  commit "$DIR/$p" "$p : début"
done
echo "Projets de démonstration prêts dans $DIR : calc, recettes, meteo, agenda."
