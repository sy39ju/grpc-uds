// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Deterministic BLE service (derives from the grpcudspp-generated
// `ble::BleService::Service`). Shared by the gg (server+client) and gt (server)
// cells; emits the same values as the Rust `ble_domain` so a tonic peer agrees.
#ifndef GRPCUDS_EXAMPLES_BLE_SERVICE_IMPL_H_
#define GRPCUDS_EXAMPLES_BLE_SERVICE_IMPL_H_

#include "ble.grpc.pb.h"

class BleServiceImpl final : public ble::BleService::Service {
 public:
    grpcuds::Status Init(grpcuds::ServerContext*, const ::ble_InitRequest*,
                         ::ble_InitReply* response) override;
    grpcuds::Status StartLeScan(grpcuds::ServerContext*, const ::ble_StartLeScanRequest*,
                                ::ble_StartLeScanReply* response) override;
    grpcuds::Status StopLeScan(grpcuds::ServerContext*, const ::ble_StopLeScanRequest*,
                               ::ble_StopLeScanReply* response) override;
    grpcuds::Status ScanResultStream(grpcuds::ServerContext*,
                                     const ::ble_ScanResultStreamRequest*,
                                     grpcuds::ServerWriter<::ble_ScanResult>* writer) override;
    grpcuds::Status AdapterStateChangeStream(
        grpcuds::ServerContext*, const ::ble_AdapterStateChangeStreamRequest*,
        grpcuds::ServerWriter<::ble_AdapterStateChange>* writer) override;
    grpcuds::Status AddScanFilter(grpcuds::ServerContext*, const ::ble_AddScanFilterRequest*,
                                  ::ble_AddScanFilterReply* response) override;
    grpcuds::Status RemoveScanFilter(grpcuds::ServerContext*, const ::ble_RemoveScanFilterRequest*,
                                     ::ble_RemoveScanFilterReply* response) override;
};

#endif  // GRPCUDS_EXAMPLES_BLE_SERVICE_IMPL_H_
