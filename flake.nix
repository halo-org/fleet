{
  description = "An AI agent harness";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    ci = {
      url = "github:halo-org/ci";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      ci,
      ...
    }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      inherit (pkgs) lib;
      craneLib = crane.mkLib pkgs;

      certificates = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";

      pluginNames = [
        "agents"
        "host"
        "openai"
        "roles"
        "sandbox"
        "sprites"
        "tools"
      ];

      common = {
        pname = "fleet";
        version = "0.0.0";
        strictDeps = true;
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.difference ./. (
            lib.fileset.unions [
              ./.github
              ./flake.lock
              ./flake.nix
            ]
          );
        };
        # aws-lc-sys builds with CMake. Wasm targets link with lld.
        nativeBuildInputs = [
          pkgs.cmake
          pkgs.lld
        ];
        dontUseCmakeConfigure = true;
        doInstallCargoArtifacts = false;
      };

      withDependencies = args: args // { cargoArtifacts = craneLib.buildDepsOnly args; };

      # The whole workspace, as developers build and test it.
      workspace = withDependencies (
        common
        // {
          cargoExtraArgs = "--locked --workspace";
          CARGO_PROFILE = "dev";
          CARGO_PROFILE_DEV_DEBUG = "false";
        }
      );

      # The server alone, so it cannot rely on features unified from other crates.
      server = withDependencies (
        common
        // {
          cargoExtraArgs = "--locked --package fleet";
          doCheck = false;
        }
      );

      guest = withDependencies (
        common
        // {
          cargoExtraArgs =
            "--locked" + lib.concatMapStrings (name: " --package fleet-plugin-${name}") pluginNames;
          CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
          doCheck = false;
        }
      );

      fleet = craneLib.buildPackage server;

      plugin =
        name:
        let
          args = withDependencies (
            common
            // {
              pname = "fleet-plugin-${name}";
              cargoExtraArgs = "--locked --package fleet-plugin-${name}";
              CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
              doCheck = false;
            }
          );
        in
        craneLib.buildPackage (
          args
          // {
            nativeBuildInputs = args.nativeBuildInputs ++ [ pkgs.wasm-tools ];
            installPhaseCommand = ''
              wasm-tools component new target/wasm32-unknown-unknown/release/fleet_plugin_${name}.wasm -o $out
            '';
          }
        );

      wasmComponents = lib.genAttrs' pluginNames (
        name: lib.nameValuePair "fleet-plugin-${name}" (plugin name)
      );

      image = ci.lib.ociImage system {
        name = "fleet";
        entrypoint = [ (lib.getExe' fleet "fleet") ];
        env = {
          SSL_CERT_FILE = certificates;
          FLEET_DIR = "/var/lib/fleet";
        };
        layers = [
          pkgs.stdenv.cc.cc.lib
          pkgs.cacert
        ];
      };
    in
    {
      packages.${system} = {
        inherit fleet;
        default = fleet;
      };

      ociImages.${system}.fleet = image;

      wasmComponents.${system} = wasmComponents;

      devShells.${system}.default = craneLib.devShell {
        packages = common.nativeBuildInputs ++ [
          pkgs.nixfmt
          pkgs.rust-analyzer
          pkgs.wasm-tools
        ];
      };

      checks.${system} = ci.lib.outputChecks self system // {
        fmt = craneLib.cargoFmt (common // { cargoExtraArgs = "--all"; });

        nix-fmt = pkgs.runCommand "fleet-nix-fmt" { nativeBuildInputs = [ pkgs.nixfmt ]; } ''
          nixfmt --check ${./flake.nix}
          touch $out
        '';

        clippy = craneLib.cargoClippy (
          workspace // { cargoClippyExtraArgs = "--all-targets -- --deny warnings"; }
        );

        clippy-plugins = craneLib.cargoClippy (guest // { cargoClippyExtraArgs = "-- --deny warnings"; });

        test = craneLib.cargoTest (workspace // { SSL_CERT_FILE = certificates; });

        doc = craneLib.cargoDoc (workspace // { RUSTDOCFLAGS = "--deny warnings"; });
      };
    };
}
