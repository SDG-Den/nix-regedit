self: {
  config,
  lib,
  pkgs,
  ...
}:
with lib; let
  cfg = config.services.nix-regedit;
  package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
in {
  options.services.nix-regedit = {
    enable = mkEnableOption "nix-regedit, a registry-editor style GUI for Nix flakes";

    package = mkOption {
      type = types.package;
      default = package;
      description = "The nix-regedit package to use.";
    };
  };

  config = mkIf cfg.enable {
    environment.systemPackages = [cfg.package];
  };
}
