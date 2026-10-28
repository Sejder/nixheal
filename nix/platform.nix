lib: build: host:
let
  plain = p: {
    config = p.config or p.system;
    gcc = p.gcc or { };
    useLLVM = p.useLLVM or false;
    linker = p.linker or "bfd";
    isStatic = p.isStatic or false;
  };
  elaborate = args: if lib == null then args else lib.systems.elaborate args;
  differing =
    p:
    let
      mine = plain p;
      usual = plain (elaborate { config = mine.config; });
    in
    builtins.removeAttrs mine (builtins.filter (n: mine.${n} == usual.${n}) (builtins.attrNames mine));
  portable = p: { config = (plain p).config; } // differing p;
  usualFor = p: lib == null || !(p ? config) || plain p == plain (elaborate { inherit (p) system; });
in
if plain build == plain host then
  if usualFor host then { inherit (host) system; } else { localSystem = portable host; }
else
  {
    localSystem = if usualFor build then { inherit (build) system; } else portable build;
    crossSystem = portable host;
  }
