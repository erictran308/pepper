use DynaLoader;
my $lib = DynaLoader::dl_load_file($ARGV[0], 0) or die "load: " . DynaLoader::dl_error();
my $sym = DynaLoader::dl_find_symbol($lib, "np_dump") or die "symbol";
DynaLoader::dl_install_xsub("main::np_dump", $sym);
np_dump();
