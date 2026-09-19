{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (lib)
    mkEnableOption
    mkIf
    mkOption
    types
    ;
  cfg = config.services.koerier;
  toml = pkgs.formats.toml { };
  credentialName = alias: kind: "node-${alias}-${kind}";
  configFile = toml.generate "koerier.toml" {
    koerier = {
      domain = cfg.publicUrl;
      bind_address = cfg.listenAddress;
      description = cfg.description;
      request_timeout_secs = cfg.requestTimeoutSecs;
      max_in_flight = cfg.maxInFlight;
    }
    // lib.optionalAttrs (cfg.imagePath != null) {
      image_path = toString cfg.imagePath;
    };
    nodes = lib.mapAttrs (alias: node: {
      rest_host = node.restHost;
      tls_cert_path = credentialName alias "tls";
      invoice_macaroon_path = credentialName alias "invoice";
      min_invoice_amount = node.minInvoiceAmount;
      max_invoice_amount = node.maxInvoiceAmount;
      invoice_expiry_sec = node.invoiceExpirySec;
    }) cfg.nodes;
  };
  credentialPath = types.strMatching "/[^\n:]+";
  nodeType = types.submodule {
    options = {
      restHost = mkOption {
        type = types.str;
        example = "127.0.0.1:8081";
        description = "LND REST socket address. Connections use HTTPS and verify its certificate.";
      };
      tlsCertPath = mkOption {
        type = credentialPath;
        description = "Absolute runtime path to the LND TLS certificate; loaded with systemd credentials.";
      };
      invoiceMacaroonPath = mkOption {
        type = credentialPath;
        description = "Absolute runtime path to an invoice-only LND macaroon; never copied into the Nix store.";
      };
      minInvoiceAmount = mkOption {
        type = types.ints.positive;
        default = 1;
        description = "Minimum invoice amount in satoshis.";
      };
      maxInvoiceAmount = mkOption {
        type = types.ints.positive;
        default = 1000000;
        description = "Maximum invoice amount in satoshis.";
      };
      invoiceExpirySec = mkOption {
        type = types.ints.positive;
        default = 600;
        description = "Invoice expiry in seconds.";
      };
    };
  };
in
{
  options.services.koerier = {
    enable = mkEnableOption "the Koerier Lightning Address server";
    package = mkOption {
      type = types.package;
      default = pkgs.callPackage ./package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix { }";
      description = "Koerier package to run.";
    };
    listenAddress = mkOption {
      type = types.str;
      default = "127.0.0.1:8090";
      description = "Private HTTP socket address. This module does not open firewall ports.";
    };
    publicUrl = mkOption {
      type = types.str;
      example = "https://lnurl.example.com";
      description = "Public HTTPS origin used for Lightning Addresses and callback URLs.";
    };
    description = mkOption {
      type = types.str;
      default = "Lightning payment";
      description = "Text description included in LNURL metadata.";
    };
    imagePath = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Optional image file readable by the service for LNURL metadata.";
    };
    requestTimeoutSecs = mkOption {
      type = types.ints.positive;
      default = 10;
      description = "Deadline in seconds for an LND request.";
    };
    maxInFlight = mkOption {
      type = types.ints.positive;
      default = 16;
      description = "Maximum number of concurrent LND requests.";
    };
    nodes = mkOption {
      type = types.attrsOf nodeType;
      default = { };
      description = "Lightning Address aliases and their backing LND nodes.";
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.nodes != { };
        message = "services.koerier.nodes must contain at least one LND node.";
      }
      {
        assertion = builtins.match "https://[^/@?#]+/?" cfg.publicUrl != null;
        message = "services.koerier.publicUrl must be an HTTPS origin without a path, query, or credentials.";
      }
    ]
    ++ lib.concatLists (
      lib.mapAttrsToList (alias: node: [
        {
          assertion = builtins.match "[a-z0-9_-]{1,64}" alias != null;
          message = "Koerier aliases must contain 1 to 64 lowercase letters, digits, underscores, or hyphens.";
        }
        {
          assertion = node.minInvoiceAmount <= node.maxInvoiceAmount;
          message = "services.koerier.nodes.${alias}: minimum invoice amount exceeds maximum.";
        }
      ]) cfg.nodes
    );

    systemd.services.koerier = {
      description = "Koerier Lightning Address server";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      serviceConfig = {
        ExecStart = "${lib.getExe cfg.package} -c ${configFile}";
        LoadCredential = lib.concatLists (
          lib.mapAttrsToList (alias: node: [
            "${credentialName alias "tls"}:${node.tlsCertPath}"
            "${credentialName alias "invoice"}:${node.invoiceMacaroonPath}"
          ]) cfg.nodes
        );
        DynamicUser = true;
        Restart = "on-failure";
        RestartSec = "5s";
        UMask = "0077";
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        RestrictSUIDSGID = true;
        RestrictRealtime = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        CapabilityBoundingSet = "";
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
        ];
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@privileged"
          "~@resources"
        ];
      };
    };
  };
}
