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
        };
      }
    );
}
