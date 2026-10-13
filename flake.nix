{
  description = "Suncord — native Discord client (Tauri 2)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };

        # Minimal runtime set for WebKitGTK + PipeWire + TLS on Linux/NixOS.
        libraries = with pkgs; [
          gtk3
          webkitgtk_4_1
          libsoup_3
          openssl
          pipewire
          alsa-lib
          libopus
          libgbm
          glib
          cairo
          pango
          gdk-pixbuf
        ];

        nativeBuild = with pkgs; [
          pkg-config
          cmake
          clang
          cargo
          rustc
          rustPlatform.bindgenHook
          nodejs_22
          cargo-tauri
        ];

        libraryPath = pkgs.lib.makeLibraryPath libraries;

        suncord = pkgs.rustPlatform.buildRustPackage {
          pname = "suncord";
          version = "0.1.0";
          src = ./.;
          cargoRoot = "src-tauri";
          buildAndTestSubdir = "src-tauri";
          cargoLock.lockFile = ./src-tauri/Cargo.lock;
          nativeBuildInputs = nativeBuild;
          buildInputs = libraries;
          preBuild = ''
            export HOME=$(mktemp -d)
            export npm_config_cache=$TMPDIR/npm-cache
            npm ci --ignore-scripts
            npm run build
          '';
          buildPhase = ''
            runHook preBuild
            export LD_LIBRARY_PATH="${libraryPath}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
            export OPENSSL_NO_VENDOR=1
            cargo build --release --manifest-path src-tauri/Cargo.toml
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            install -Dm755 src-tauri/target/release/suncord $out/bin/suncord
            runHook postInstall
          '';
          meta = with pkgs.lib; {
            description = "Native desktop Discord client";
            mainProgram = "suncord";
            platforms = platforms.linux;
          };
        };
      in
      {
        packages.default = suncord;
        packages.suncord = suncord;
        apps.default = {
          type = "app";
          program = "${suncord}/bin/suncord";
        };
        devShells.default = pkgs.mkShell {
          name = "suncord";
          buildInputs = nativeBuild ++ libraries;
          LD_LIBRARY_PATH = libraryPath;
          OPENSSL_NO_VENDOR = "1";
          shellHook = ''
            echo "Suncord: npm install && npm run tauri dev  |  nix run"
          '';
        };
      }
    );
}
