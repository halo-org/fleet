{
  description = "An AI agent harness";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    { nixpkgs, crane, ... }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      inherit (pkgs) lib;
      craneLib = crane.mkLib pkgs;

      certificates = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";

      pluginNames = [
        "host"
        "openai"
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

      plugins = craneLib.buildPackage (
        guest
        // {
          pname = "fleet-plugins";
          nativeBuildInputs = guest.nativeBuildInputs ++ [ pkgs.wasm-tools ];
          installPhaseCommand = ''
            mkdir -p $out/share/fleet/plugins
            for name in ${toString pluginNames}; do
              wasm-tools component new \
                target/wasm32-unknown-unknown/release/fleet_plugin_$name.wasm \
                -o $out/share/fleet/plugins/$name.wasm
            done
          '';
        }
      );

      image = pkgs.dockerTools.streamLayeredImage {
        name = "fleet";
        tag = "latest";
        config = {
          Entrypoint = [ (lib.getExe' fleet "fleet") ];
          Env = [
            "SSL_CERT_FILE=${certificates}"
            "FLEET_DIR=/var/lib/fleet"
          ];
        };
      };

      # Usage: REGISTRY_USER=... REGISTRY_PASSWORD=... publish ghcr.io/owner/name:tag
      publish = pkgs.writeShellApplication {
        name = "publish";
        runtimeInputs = [ pkgs.skopeo ];
        text = ''
          ${image} | skopeo --insecure-policy copy \
            --dest-creds "$REGISTRY_USER:$REGISTRY_PASSWORD" \
            docker-archive:/dev/stdin "docker://$1"
        '';
      };
    in
    {
      packages.${system} = {
        inherit
          fleet
          plugins
          image
          publish
          ;
        default = fleet;
      };

      devShells.${system}.default = craneLib.devShell {
        packages = common.nativeBuildInputs ++ [
          pkgs.nixfmt
          pkgs.rust-analyzer
          pkgs.wasm-tools
        ];
      };

      checks.${system} = {
        inherit fleet plugins image;

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
