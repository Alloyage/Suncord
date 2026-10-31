{ pkgs }:
let
  python = pkgs.python3.withPackages (p: [ p.fonttools p.brotli ]);
  sources = pkgs.lib.mapAttrs (_: source: pkgs.fetchurl {
    inherit (source) url;
    sha256 = source.sha256;
  }) (builtins.fromJSON (builtins.readFile ./sources.json));
in
pkgs.runCommand "suncord-assets" { nativeBuildInputs = [ python ]; } ''
  python ${../tools/assets.py} --sources ${pkgs.writeText "suncord-asset-sources.json" (builtins.toJSON sources)} \
    --manifest ${./sources.json} --emoji ${./emoji/emojis.tsv} $out
''
