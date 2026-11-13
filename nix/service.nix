# The parts of the systemd user unit that NixOS and home-manager agree on.
# Both modules option-ify the same `cfg`; only the surrounding module plumbing
# and the attribute spelling (lowercase vs. Unit/Service/Install) differ.
{ lib, pkgs, cfg }:

let
  tomlFormat = pkgs.formats.toml { };

  configFile =
    if cfg.configFile != null then cfg.configFile
    else if cfg.settings != { } then tomlFormat.generate "npd-config.toml" cfg.settings
    else null;

  execStart = lib.concatStringsSep " " (
    [ (lib.getExe cfg.package) "run" ]
    ++ lib.optionals (configFile != null) [ "--config" "${configFile}" ]
    ++ lib.concatMap (addr: [ "--bind" addr ]) cfg.bind
  );
in
{
  inherit configFile;

  description = "npd - MPD-compatible daemon for Navidrome";

  # npd streams over the network and plays through PipeWire, so it wants both
  # up first. Neither is a hard dependency: it retries, and a missing sink only
  # fails playback, not startup.
  after = [ "network-online.target" "pipewire.service" ];
  wants = [ "network-online.target" ];

  serviceConfig = {
    ExecStart = execStart;
    Restart = "on-failure";
    RestartSec = 5;
  } // lib.optionalAttrs (cfg.logLevel != null) {
    Environment = [ "NPD_LOG=${cfg.logLevel}" ];
  };

  wantedBy = [ "default.target" ];
}
