# SPDX-License-Identifier: MIT OR Apache-2.0
grpcuds_codegen(x509)

grpcuds_example(x509-gg BOTH x509 gg/main.cc x509_service_impl.cc)
if(TARGET x509-gg)
    add_test(NAME x509-gg COMMAND x509-gg)
endif()

grpcuds_example(x509-gt-server SERVER x509 gt/server_main.cc x509_service_impl.cc)

grpcuds_example(x509-tg-client CLIENT x509 tg/client_main.cc)
