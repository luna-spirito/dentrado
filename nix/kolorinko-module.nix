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
  credentialsDir = "/run/credentials/kolorinko.service";
  configFile =
    let
      # Paths the module owns: mutable state, the dist inside the package,
      # and TLS material exposed as systemd credentials (see LoadCredential).
      managed = {
        repo.dir = "/var/lib/kolorinko/repo";
        server.web_dist = "${cfg.package}/share/web-dist";
        server.cert_file = "${credentialsDir}/cert";
        server.key_file = "${credentialsDir}/key";
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

    certs = {
      cert = lib.mkOption {
        type = lib.types.str;
        default = "/etc/kolorinko/cert.pem";
        description = ''
          TLS certificate file. Read by systemd (as root, so any permission
          works) and exposed to the service as a credential; the server sees it
          at `/run/credentials/kolorinko.service/cert` regardless of this path.
        '';
      };
      key = lib.mkOption {
        type = lib.types.str;
        default = "/etc/kolorinko/key.pem";
        description = ''
          TLS private key file. Read by systemd (as root) and exposed to the
          service as a credential; the server sees it at
          `/run/credentials/kolorinko.service/key` regardless of this path.
        '';
      };
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
        Contents of kolorinko's TOML config. `repo.dir`, `server.web_dist`,
        `server.cert_file` and `server.key_file` are derived from `package`,
        the service's state directory and the loaded credentials; setting
        them here has no effect.
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
      documentation = [ "https://github.com/luna-spirito/dentrado" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];

      environment.RUST_LOG = lib.mkDefault "info";

      serviceConfig = {
        ExecStart = "${cfg.package}/bin/kolorinko ${configFile}";
        # PID 1 (root) reads the sources and republishes them per-start in
        # /run/credentials, owned by the dynamic user — private keys can keep
        # root-only permissions on disk and never touch the service's view.
        LoadCredential = [
          "cert:${cfg.certs.cert}"
          "key:${cfg.certs.key}"
        ];
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
