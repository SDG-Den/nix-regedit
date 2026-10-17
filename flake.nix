{
  description = "nix-regedit: a registry-editor style GUI for Nix flakes";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs = {
    self,
    nixpkgs,
    flake-utils,
    crane,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (
      system: let
        pkgs = import nixpkgs {inherit system;};
        inherit (pkgs) lib;

        craneLib = crane.mkLib pkgs;

        runtimePath = lib.makeBinPath (with pkgs; [
          nix
          nixd
          nixfmt
        ]);

        nativeBuildInputs = with pkgs; [
          pkg-config
          wrapGAppsHook4
        ];

        buildInputs = with pkgs; [
          gtk4
          libadwaita
          glib
        ];

        commonArgs = {
          pname = "nix-regedit";
          version = "0.1.0";
          src = lib.cleanSource ./.;
          inherit nativeBuildInputs buildInputs;
          doCheck = false;
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        nix-regedit = craneLib.buildPackage (commonArgs
          // {
            inherit cargoArtifacts;

            preFixup = ''
              gappsWrapperArgs+=(--prefix PATH : "${runtimePath}")
            '';

            postInstall = ''
              install -Dm644 ${./nix-regedit.desktop} \
                "$out/share/applications/nix-regedit.desktop"
            '';

            meta = with lib; {
              description = "A registry-editor style GUI for Nix flakes";
              mainProgram = "nix-regedit";
              license = licenses.gpl3Plus;
              platforms = platforms.linux;
            };
          });
      in {
        packages = {
          inherit nix-regedit;
          default = nix-regedit;
        };

        apps.default = {
          type = "app";
          program = "${nix-regedit}/bin/nix-regedit";
        };

        checks.default = nix-regedit;

        devShells.default = pkgs.mkShell {
          nativeBuildInputs =
            nativeBuildInputs
            ++ (with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              rust-analyzer
              mold
            ]);

          buildInputs =
            buildInputs
            ++ (with pkgs; [
              nix
              nixd
              nixfmt
            ]);

          RUST_SRC_PATH = pkgs.rustPlatform.rustLibSrc;
          RUSTFLAGS = "-C link-arg=-fuse-ld=mold";
        };
      }
    )
    // {
      nixosModules.default = import ./modules/nixos.nix self;
      nixosModules.nix-regedit = import ./modules/nixos.nix self;
    };
}
