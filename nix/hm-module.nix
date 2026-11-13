# home-manager module. Same options as the NixOS one, scoped to this user.
{ self }:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.npd;
  service = import ./service.nix { inherit lib pkgs cfg; };
in
{
  options.services.npd = import ./options.nix {
    inherit lib pkgs;
    defaultPackage = self.packages.${pkgs.stdenv.hostPlatform.system}.npd;
  };

  config = lib.mkIf cfg.enable {
    assertions = [{
      assertion = !(cfg.configFile != null && cfg.settings != { });
      message = "services.npd: set either settings or configFile, not both.";
    }];

    home.packages = [ cfg.package ];

    systemd.user.services.npd = {
      Unit = {
        Description = service.description;
        After = service.after;
        Wants = service.wants;
      };
      Service = service.serviceConfig;
      Install.WantedBy = service.wantedBy;
    };
  };
}
