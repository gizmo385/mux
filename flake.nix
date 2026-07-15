{
  description = "agent-mux — terminal multiplexer for Claude Code conversations";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    let
      mkAgentMux =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "agent-mux";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # On aarch64-darwin, some nixpkgs revisions ship a legacy cctools
          # `ld` that SIGTRAPs (Trace/BPT trap: 5 → exit 133) while linking
          # this binary — it takes down `nix build` for any downstream flake
          # consumer even though the Rust code is fine. Route the final link
          # through LLVM's lld (already in the closure) instead. Darwin-only;
          # no effect on Linux, where the default linker links cleanly.
          nativeBuildInputs = pkgs.lib.optionals pkgs.stdenv.isDarwin [ pkgs.lld ];
          RUSTFLAGS = pkgs.lib.optionalString pkgs.stdenv.isDarwin "-C link-arg=-fuse-ld=lld";
          # Tests shell out to git/ssh and assume a real environment (cf. host::tests
          # exercising ControlMaster lifecycle). They're CI's job — release.yml builds
          # without tests too. The flake produces a binary; the binary's correctness
          # is gated upstream.
          doCheck = false;
          meta = with pkgs.lib; {
            description = "Fast terminal-first multiplexer for Claude Code conversations across local and remote hosts";
            homepage = "https://github.com/gizmo385/mux";
            license = licenses.mit;
            mainProgram = "agent-mux";
            platforms = platforms.unix;
          };
        };
    in
    {
      overlays.default = final: _prev: {
        agent-mux = mkAgentMux final;
      };
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        agent-mux = mkAgentMux pkgs;
      in
      {
        packages = {
          default = agent-mux;
          agent-mux = agent-mux;
        };

        apps.default = {
          type = "app";
          program = "${agent-mux}/bin/agent-mux";
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
          ];
          # On macOS, nix's stdenv sets DEVELOPER_DIR to its bundled Apple
          # SDK. Apple's /usr/bin/python3 is a stub that resolves the real
          # interpreter via DEVELOPER_DIR, and the nix SDK has no python3
          # — so any python script invoked from inside the devShell (e.g.
          # the user's Coder CLI wrapper used as an SSH ProxyCommand for
          # *.coder hosts) fails with `error: tool 'python3' not found`,
          # which surfaces as agent-mux's remote-host connect dying with
          # exit 255. Unsetting lets the stub fall back to Command Line
          # Tools; the nix cc-wrapper has its own header paths baked into
          # NIX_CFLAGS_COMPILE so cargo builds (including objc2-* / Cocoa
          # framework crates) still link cleanly. No-op on Linux.
          shellHook = ''
            unset DEVELOPER_DIR
          '';
        };
      }
    );
}
