{ pkgs, lib, ... }:

{
  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
  };

  # bubblewrap pins builds to one path (Linux only); cargo-nextest runs `test --affected` in tests;
  # jq and bc run the benchmark scripts in bench/.
  packages = [ pkgs.jujutsu pkgs.jq pkgs.bc pkgs.rsync pkgs.cargo-nextest ]
    ++ lib.optionals pkgs.stdenv.isLinux [ pkgs.bubblewrap ];

  scripts.check.exec = "cargo fmt --all --check && cargo clippy --all-targets --locked -- -D warnings";
  # The macOS and Windows code paths compile and lint clean, checked from Linux.
  scripts.check-targets.exec = ''
    for target in aarch64-apple-darwin x86_64-pc-windows-gnu; do
      cargo clippy --target "$target" --all-targets --locked -- -D warnings || exit 1
    done
  '';

  enterShell = ''
    echo "Agent Hearth devenv: cargo test, check."
  '';

  enterTest = ''
    set -euo pipefail
    cargo fmt --all --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked
    check-targets
  '';
}
