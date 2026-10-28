build: host:
let
  key = p: {
    config = p.config or p.system;
    static = p.isStatic or false;
  };
in
if key build == key host then
  { inherit (host) system; }
else
  {
    localSystem = { inherit (build) system; };
    crossSystem = {
      config = host.config;
    }
    // (if host.isStatic or false then { isStatic = true; } else { });
  }
