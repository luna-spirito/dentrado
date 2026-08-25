# NixOS module: kolorinko as a systemd service.
#
# The default package is this flake's pure crane build (see
# nix/kolorinko-package.nix): server binary + Trunk-built web assets in one
# closure.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.kolorinko;

  toml = pkgs.formats.toml { };
  configFile =
    let
      # Paths the module owns: mutable state and the dist inside the package.
      managed = {
        repo.dir = "/var/lib/kolorinko/repo";
        server.web_dist = "${cfg.package}/share/web-dist";
      };
    in
    toml.generate "kolorinko.toml" (lib.recursiveUpdate cfg.settings managed);
in
{
  options.services.kolorinko = {
    enable = lib.mkEnableOption "kolorinko, the HTTP/3 + WebTransport wiki mirror";

    package = lib.mkOption {
      type = lib.types.package;
      description = "kolorinko package to run. The flake's `nixosModules.kolorinko` wrapper defaults this to its own `packages.<system>.kolorinko`; set it explicitly only when importing the module file directly.";
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open the firewall (TCP + UDP/QUIC) for `settings.server.bind`'s port.";
    };

    settings = lib.mkOption {
      type = toml.type;
      default = { };
      example = {
        repo = {
          url = "https://github.com/luna-spirito/wikidot-kolorinko-export.git";
          interval = 900;
        };
        server = {
          bind = "[::]:443";
          inject_wt_hash = false;
          cert_file = "/etc/kolorinko/cert.pem";
          key_file = "/etc/kolorinko/key.pem";
        };
        ensure-evakuilo-sites."obscurative" = {
          landing = "main";
          domains = [ "www.obscurative.ru" ];
        };
      };
      description = ''
        Contents of kolorinko's TOML config. `repo.dir` and
        `server.web_dist` are derived from `package` and the service's state
        directory and must not be set here.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    networking.firewall = lib.mkIf cfg.openFirewall {
      allowedTCPPorts = [ (lib.toInt (lib.last (lib.splitString ":" cfg.settings.server.bind))) ];
      allowedUDPPorts = [ (lib.toInt (lib.last (lib.splitString ":" cfg.settings.server.bind))) ];
    };

    systemd.services.kolorinko = {
      description = "kolorinko (HTTP/3 + WebTransport wiki mirror)";
      documentation = "https://github.com/luna-spirito/dentrado";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];

      environment.RUST_LOG = lib.mkDefault "info";

      serviceConfig = {
        ExecStart = "${cfg.package}/bin/kolorinko ${configFile}";
        StateDirectory = "kolorinko";
        WorkingDirectory = "/var/lib/kolorinko";
        DynamicUser = true;
        # Binding 443.
        AmbientCapabilities = [ "CAP_NET_BIND_SERVICE" ];
        CapabilityBoundingSet = [ "CAP_NET_BIND_SERVICE" ];
        # io_uring buffer registration.
        LimitMEMLOCK = "infinity";
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
        ];
        RestrictNamespaces = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        NoNewPrivileges = true;
        Restart = "on-failure";
      };
    };
  };
}
