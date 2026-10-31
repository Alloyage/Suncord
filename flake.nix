{
  description = "Suncord — browser Discord client with a local Rust backend";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # This flake packages the x86-64 Linux build.
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      libdave = pkgs.stdenv.mkDerivation {
        pname = "libdave";
        version = "1.2.1";
        src = pkgs.fetchurl {
          url = "https://github.com/discord/libdave/releases/download/v1.2.1/cpp/libdave-Linux-X64-boringssl.zip";
          hash = "sha256-f98Qp9+JRAYpFGMTCjIsBNfeWAhbYCUAhmoRh06JB7Q=";
        };
        nativeBuildInputs = [ pkgs.unzip pkgs.autoPatchelfHook ];
        buildInputs = [ pkgs.stdenv.cc.cc.lib ];
        sourceRoot = ".";
        dontBuild = true;
        installPhase = ''
          runHook preInstall
          mkdir -p $out
          cp -r lib include $out/
          mkdir -p $out/share/licenses/libdave
          cp -r licenses/* $out/share/licenses/libdave/
          runHook postInstall
        '';
        meta.platforms = [ system ];
      };
      # Raw PipeWire frames can lack DTS. Keepalive must preserve their stream clock.
      pipewire = pkgs.pipewire.overrideAttrs (old: {
        patches = (old.patches or [ ]) ++ [ ./patches/pipewire-keepalive-timestamps.patch ];
      });
      assets = import ./assets { inherit pkgs; };
      nativeMediaPlugins = with pkgs.gst_all_1; [ (pkgs.lib.getLib gstreamer) gst-plugins-base gst-plugins-good gst-plugins-ugly ];
      runtimeLibraries = with pkgs; [ dbus gst_all_1.gstreamer gst_all_1.gst-plugins-base libdave ];
      nativeMediaPluginPath = pkgs.lib.makeSearchPath "lib/gstreamer-1.0" (nativeMediaPlugins ++ [ pipewire ]);
      hardwareMediaPluginPath = pkgs.lib.makeSearchPath "lib/gstreamer-1.0" [ pkgs.gst_all_1.gst-plugins-bad ];
      browser = pkgs.rustPlatform.buildRustPackage {
        pname = "suncord-browser";
        version = "0.0.0";
        src = ./.;
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [ pkgs.pkg-config pkgs.makeWrapper ];
        buildInputs = runtimeLibraries;
        env.CPLUS_INCLUDE_PATH = "${libdave}/include";
        env.SUNCORD_ASSETS = assets;
        doCheck = false;
        postInstall = ''
          mkdir -p $out/share/licenses/suncord-browser
          cp ${assets}/licenses/* $out/share/licenses/suncord-browser/
          cp assets/emoji/LICENSE $out/share/licenses/suncord-browser/emoji.LICENSE
          cp -r ${libdave}/share/licenses/libdave $out/share/licenses/suncord-browser/
        '';
        postFixup = ''
          wrapProgram $out/bin/suncord-browser \
            --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath runtimeLibraries}" \
            --prefix GST_PLUGIN_SYSTEM_PATH_1_0 : "${nativeMediaPluginPath}:${hardwareMediaPluginPath}"
        '';
        meta = { mainProgram = "suncord-browser"; platforms = [ system ]; };
      };
    in
    {
      packages.${system}.default = browser;
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [ cargo rustc rustfmt clippy pkg-config ];
        buildInputs = runtimeLibraries;
        CPLUS_INCLUDE_PATH = "${libdave}/include";
        SUNCORD_ASSETS = assets;
        GST_PLUGIN_SYSTEM_PATH_1_0 = "${nativeMediaPluginPath}:${hardwareMediaPluginPath}";
        LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibraries;
      };
    };
}
