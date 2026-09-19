{ nixpkgs, pkgs }:

let
  machine = nixpkgs.lib.nixosSystem {
    system = pkgs.stdenv.hostPlatform.system;
    modules = [
      ../module.nix
      {
        system.stateVersion = "26.05";
        boot.isContainer = true;
        services.koerier = {
          enable = true;
          package = pkgs.writeShellScriptBin "koerier" "exit 0";
          publicUrl = "https://lnurl.example.com";
          nodes.thor = {
            restHost = "127.0.0.1:8081";
            tlsCertPath = "/run/lnd/thor/tls.cert";
            invoiceMacaroonPath = "/run/lnd/thor/invoice.macaroon";
          };
        };
      }
    ];
  };
  config = machine.config;
  service = config.systemd.services.koerier.serviceConfig;
  configFile = nixpkgs.lib.last (nixpkgs.lib.splitString " " service.ExecStart);
in
assert nixpkgs.lib.all (item: item.assertion) config.assertions;
assert service.DynamicUser;
assert service.ProtectSystem == "strict";
assert
  service.LoadCredential == [
    "node-thor-tls:/run/lnd/thor/tls.cert"
    "node-thor-invoice:/run/lnd/thor/invoice.macaroon"
  ];
assert config.networking.firewall.allowedTCPPorts == [ ];
pkgs.runCommand "koerier-module-check"
  {
    nativeBuildInputs = [ pkgs.python3 ];
  }
  ''
    python - ${configFile} <<'PY'
    import sys
    import tomllib

    with open(sys.argv[1], "rb") as source:
        config = tomllib.load(source)
    assert config["koerier"]["domain"] == "https://lnurl.example.com"
    assert config["koerier"]["bind_address"] == "127.0.0.1:8090"
    node = config["nodes"]["thor"]
    assert node["rest_host"] == "127.0.0.1:8081"
    assert node["tls_cert_path"] == "node-thor-tls"
    assert node["invoice_macaroon_path"] == "node-thor-invoice"
    assert node["min_invoice_amount"] == 1
    assert node["max_invoice_amount"] == 1000000
    assert node["invoice_expiry_sec"] == 600
    assert "/run/lnd" not in str(config)
    PY
    touch "$out"
  ''
