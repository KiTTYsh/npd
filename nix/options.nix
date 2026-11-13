# Options shared by the NixOS and home-manager modules.
{ lib, pkgs, defaultPackage }:

let
  tomlFormat = pkgs.formats.toml { };
in
{
  enable = lib.mkEnableOption "npd, an MPD-compatible daemon for Navidrome";

  package = lib.mkOption {
    type = lib.types.package;
    default = defaultPackage;
    defaultText = lib.literalMD "the `npd` package from this flake";
    description = "The npd package to run.";
  };

  settings = lib.mkOption {
    type = tomlFormat.type;
    default = { };
    example = lib.literalExpression ''
      {
        server = {
          url = "https://music.example.com";
          username = "me";
          password_command = "pass show navidrome";
        };
        mpd.socket = "$XDG_RUNTIME_DIR/mpd/socket";
        playback.local_root = "/mnt/music";
      }
    '';
    description = ''
      Contents of npd's `config.toml`, written to the Nix store and passed with
      `--config`. See `contrib/config.example.toml` in the npd package's
      `share/doc/npd` for every key.

      The store is world-readable: use `server.password_command` rather than
      `server.password` to keep the password out of it.

      Leave this empty to let npd read `$XDG_CONFIG_HOME/npd/config.toml`
      itself.
    '';
  };

  configFile = lib.mkOption {
    type = lib.types.nullOr lib.types.path;
    default = null;
    example = "/var/lib/secrets/npd.toml";
    description = ''
      Config file to pass with `--config`, for configs kept out of the Nix
      store (agenix, sops-nix, a hand-written file). Overrides
      {option}`settings`.
    '';
  };

  bind = lib.mkOption {
    type = lib.types.listOf lib.types.str;
    default = [ ];
    example = [ "127.0.0.1:6600" ];
    description = ''
      Addresses to listen on, passed as `--bind`. Overrides `mpd.bind` from
      the config file when non-empty.
    '';
  };

  logLevel = lib.mkOption {
    type = lib.types.nullOr lib.types.str;
    default = null;
    example = "npd=debug";
    description = "Value for `NPD_LOG`, npd's `tracing` env filter.";
  };
}
