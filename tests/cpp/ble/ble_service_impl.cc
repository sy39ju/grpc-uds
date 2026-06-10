// SPDX-License-Identifier: MIT OR Apache-2.0
#include "ble_service_impl.h"

#include <cstdio>

#include "mock_values.h"

grpcuds::Status BleServiceImpl::Init(grpcuds::ServerContext*, const ::ble_InitRequest*,
                                     ::ble_InitReply* response) {
    response->ok = true;
    return grpcuds::Status::Ok();
}

grpcuds::Status BleServiceImpl::StartLeScan(grpcuds::ServerContext*,
                                            const ::ble_StartLeScanRequest*,
                                            ::ble_StartLeScanReply* response) {
    response->ok = true;
    return grpcuds::Status::Ok();
}

grpcuds::Status BleServiceImpl::StopLeScan(grpcuds::ServerContext*, const ::ble_StopLeScanRequest*,
                                           ::ble_StopLeScanReply* response) {
    response->ok = true;
    return grpcuds::Status::Ok();
}

grpcuds::Status BleServiceImpl::ScanResultStream(
    grpcuds::ServerContext*, const ::ble_ScanResultStreamRequest*,
    grpcuds::ServerWriter<::ble_ScanResult>* writer) {
    for (int i = 0; i < mock::kBleScanCount; ++i) {
        ::ble_ScanResult r = ble_ScanResult_init_zero;
        std::snprintf(r.mac, sizeof(r.mac), "%s", mock::kBleMac);
        r.rssi = mock::ble_rssi(i);
        r.adv_data.size = 3;
        r.adv_data.bytes[0] = mock::kBleAdv[0];
        r.adv_data.bytes[1] = mock::kBleAdv[1];
        r.adv_data.bytes[2] = mock::kBleAdv[2];
        writer->Write(r);
    }
    writer->Finish(grpcuds::Status::Ok());
    return grpcuds::Status::Ok();
}

grpcuds::Status BleServiceImpl::AdapterStateChangeStream(
    grpcuds::ServerContext*, const ::ble_AdapterStateChangeStreamRequest*,
    grpcuds::ServerWriter<::ble_AdapterStateChange>* writer) {
    writer->Finish(grpcuds::Status::Ok());
    return grpcuds::Status::Ok();
}

grpcuds::Status BleServiceImpl::AddScanFilter(grpcuds::ServerContext*,
                                              const ::ble_AddScanFilterRequest*,
                                              ::ble_AddScanFilterReply* response) {
    response->filter_id = 7;
    return grpcuds::Status::Ok();
}

grpcuds::Status BleServiceImpl::RemoveScanFilter(grpcuds::ServerContext*,
                                                 const ::ble_RemoveScanFilterRequest* request,
                                                 ::ble_RemoveScanFilterReply* response) {
    if (request->filter_id != 7) {
        return grpcuds::Status(grpcuds::NOT_FOUND, "unknown filter id");
    }
    response->ok = true;
    return grpcuds::Status::Ok();
}
