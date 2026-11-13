# NixOS module: a systemd *user* service, so npd lands in the session that owns
# the PipeWire sink it plays to. It is defined for every user who logs in; use
# the home-manager module instead to scope it to one user.
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

    environment.systemPackages = [ cfg.package ];

    systemd.user.services.npd = {
      inherit (service) description after wants serviceConfig wantedBy;
    };
  };
}
