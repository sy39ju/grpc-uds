// SPDX-License-Identifier: MIT OR Apache-2.0
//! `protoc-gen-grpcudspp` — protoc plugin emitting grpcuds service stubs
//! over nanopb message types, in **two output modes**: C++ by default, or
//! plain C with `--grpcudspp_opt=c`.
//!
//! Invocation:
//!
//! ```text
//! protoc --plugin=protoc-gen-grpcudspp=./protoc-gen-grpcudspp \
//!        --grpcudspp_out=./out [--grpcudspp_opt=c] \
//!        my.proto
//! ```
//!
//! For each input `.proto` that declares services, two files are emitted.
//!
//! **C++ mode (default)** — links the header-only `grpcudspp` wrapper:
//!
//! ```text
//! <name>.grpc.pb.h    Service base classes + typed client Stub (NewStub)
//!                     + ServerWriter<T> decls
//! <name>.grpc.pb.cc   BindToServer + trampolines + ServerWriter defs
//! ```
//!
//! **C mode (`--grpcudspp_opt=c`)** — links only the C ABI (`grpcuds.h`),
//! no C++:
//!
//! ```text
//! <name>.grpcuds.h    <Svc>_service handler table + <Svc>_register() +
//!                     typed client wrappers (static inline)
//! <name>.grpcuds.c    server trampolines + registration
//! ```
//!
//! Both modes expect nanopb's `<name>.pb.h` alongside (run
//! `nanopb_generator` on the same .proto). Trampolines call `pb_decode` /
//! `pb_encode` against the nanopb structs and forward to
//! `grpcuds_call_write` / `grpcuds_call_finish`. Client-streaming and
//! bidirectional RPCs are rejected at build time in both modes (the runtime
//! is unary + server-streaming).

use std::fmt::Write as _;
use std::io::{Read, Write};

use protobuf::descriptor::{
    DescriptorProto, FieldDescriptorProto, FileDescriptorProto, MethodDescriptorProto,
    ServiceDescriptorProto,
};
use protobuf::plugin::code_generator_response::File as ResponseFile;
use protobuf::plugin::{CodeGeneratorRequest, CodeGeneratorResponse};
use protobuf::Message;

fn main() -> std::io::Result<()> {
    // 1. Read CodeGeneratorRequest from stdin.
    let mut buf = Vec::new();
    std::io::stdin().read_to_end(&mut buf)?;
    let request = match CodeGeneratorRequest::parse_from_bytes(&buf) {
        Ok(r) => r,
        Err(e) => {
            let mut resp = CodeGeneratorResponse::new();
            resp.set_error(format!("could not parse CodeGeneratorRequest: {e}"));
            std::io::stdout().write_all(&resp.write_to_bytes()?)?;
            return Ok(());
        }
    };

    // 2. Generate for each input file. `--grpcudspp_opt=c` switches from
    //    the default C++ stubs to plain-C stubs (<base>.grpcuds.{h,c}).
    let c_mode = request.parameter().split(',').any(|opt| opt.trim() == "c");
    let mut response = CodeGeneratorResponse::new();
    let want: std::collections::HashSet<&str> = request
        .file_to_generate
        .iter()
        .map(String::as_str)
        .collect();

    for proto in &request.proto_file {
        // Only emit for files explicitly listed in file_to_generate so we
        // don't accidentally emit for transitively-imported deps.
        if !want.contains(proto.name()) {
            continue;
        }
        if proto.service.is_empty() {
            continue;
        }
        let generated = if c_mode {
            generate_file_c(proto)
        } else {
            generate_file(proto)
        };
        match generated {
            Ok((header, source)) => {
                response.file.push(header);
                response.file.push(source);
            }
            Err(e) => {
                response.set_error(format!("{}: {e}", proto.name()));
                break;
            }
        }
    }

    // 3. Write CodeGeneratorResponse to stdout.
    std::io::stdout().write_all(&response.write_to_bytes()?)?;
    Ok(())
}

/// Emit the .grpc.pb.h / .grpc.pb.cc pair for a single source proto.
fn generate_file(proto: &FileDescriptorProto) -> Result<(ResponseFile, ResponseFile), String> {
    let name = proto.name();
    let base = name
        .strip_suffix(".proto")
        .ok_or_else(|| format!("file name does not end in .proto: {name}"))?;

    let header_name = format!("{base}.grpc.pb.h");
    let source_name = format!("{base}.grpc.pb.cc");
    let nanopb_header = format!("{base}.pb.h");

    let pkg = proto.package();

    // Chainable field-setter wrappers (<Msg>Mut). Built first so we know
    // whether the header needs <cstring>/<cstddef> for string / bytes setters.
    let mut mutators = String::new();
    for desc in &proto.message_type {
        write_one_mutator(&mut mutators, pkg, &[], desc);
    }

    let mut h = String::new();
    let mut s = String::new();
    write_header_prologue(
        &mut h,
        &header_name,
        &nanopb_header,
        name,
        !mutators.is_empty(),
    )?;
    write_source_prologue(&mut s, &header_name, name)?;

    let writer_types = collect_resp_types(&proto.service, pkg, true);
    let responder_types = collect_resp_types(&proto.service, pkg, false);

    // Specialization DECLARATIONS must precede the service classes: the
    // deferred unary default implementations call Respond()/Fail() from
    // inside the class bodies, and an explicit specialization declared
    // after that instantiation point is ill-formed.
    if !writer_types.is_empty() || !responder_types.is_empty() {
        writeln!(h, "namespace grpcuds {{").unwrap();
        writeln!(h).unwrap();
        for t in &writer_types {
            writeln!(
                h,
                "template <> bool ServerWriter<{}>::Write(const {}& message);",
                t, t
            )
            .unwrap();
        }
        for t in &responder_types {
            writeln!(
                h,
                "template <> bool UnaryResponder<{}>::Respond(const {}& response);",
                t, t
            )
            .unwrap();
        }
        writeln!(h).unwrap();
        writeln!(h, "}}  // namespace grpcuds").unwrap();
        writeln!(h).unwrap();
    }

    if !pkg.is_empty() {
        let parts: Vec<&str> = pkg.split('.').collect();
        for p in &parts {
            writeln!(h, "namespace {} {{", p).unwrap();
            writeln!(s, "namespace {} {{", p).unwrap();
        }
        writeln!(h).unwrap();
        writeln!(s).unwrap();
    }

    for svc in &proto.service {
        write_service_header(&mut h, svc, pkg)?;
        write_service_source(&mut s, svc, pkg)?;
    }

    if !mutators.is_empty() {
        h.push_str(&mutators);
    }

    if !pkg.is_empty() {
        for p in pkg.split('.').rev() {
            writeln!(h, "}}  // namespace {}", p).unwrap();
            writeln!(s, "}}  // namespace {}", p).unwrap();
        }
        writeln!(h).unwrap();
        writeln!(s).unwrap();
    }

    if !writer_types.is_empty() || !responder_types.is_empty() {
        writeln!(s, "namespace grpcuds {{").unwrap();
        for t in &writer_types {
            write_server_writer_spec(&mut s, t);
        }
        for t in &responder_types {
            write_unary_responder_spec(&mut s, t);
        }
        writeln!(s, "}}  // namespace grpcuds").unwrap();
    }

    write_header_epilogue(&mut h, &header_name);

    let mut h_file = ResponseFile::new();
    h_file.set_name(header_name);
    h_file.set_content(h);

    let mut s_file = ResponseFile::new();
    s_file.set_name(source_name);
    s_file.set_content(s);

    Ok((h_file, s_file))
}

/// Emit the plain-C stub pair (.grpcuds.h / .grpcuds.c) for one proto —
/// `--grpcudspp_opt=c`. Design (mirrors docs/C_API_GUIDE.md):
///   * one `<prefix>_service` struct of handler function pointers + a
///     `<prefix>_register()` that wires every non-NULL one,
///   * generated trampolines do the nanopb decode/encode (the glue a C
///     consumer would otherwise hand-write),
///   * everything CLIENT-side (typed call wrappers, stream readers) and
///     the small server helpers (_respond/_send/_finish) are `static
///     inline` in the HEADER, so server-only or client-only libraries
///     never see undefined symbols for the half they don't link —
///     the .c file holds only server trampolines + registration.
fn generate_file_c(proto: &FileDescriptorProto) -> Result<(ResponseFile, ResponseFile), String> {
    let name = proto.name();
    let base = name
        .strip_suffix(".proto")
        .ok_or_else(|| format!("file name does not end in .proto: {name}"))?;
    let header_name = format!("{base}.grpcuds.h");
    let source_name = format!("{base}.grpcuds.c");
    let pkg = proto.package();

    let mut h = String::new();
    let guard = guard_for(&header_name);
    writeln!(
        h,
        "// Generated by protoc-gen-grpcudspp (C mode) from {name}. DO NOT EDIT."
    )
    .unwrap();
    writeln!(h, "//").unwrap();
    writeln!(
        h,
        "// Plain-C service stubs over the grpcuds C ABI. Server side: fill a"
    )
    .unwrap();
    writeln!(
        h,
        "// <Service>_service table and _register() it — the trampolines in"
    )
    .unwrap();
    writeln!(
        h,
        "// {source_name} do the nanopb decode/encode. Client side: typed call"
    )
    .unwrap();
    writeln!(
        h,
        "// wrappers, static inline below. Compile {source_name} into SERVER"
    )
    .unwrap();
    writeln!(h, "// binaries only; clients need just this header.").unwrap();
    writeln!(h, "#ifndef {guard}").unwrap();
    writeln!(h, "#define {guard}").unwrap();
    writeln!(h).unwrap();
    writeln!(h, "#include <stddef.h>").unwrap();
    writeln!(h, "#include <stdint.h>").unwrap();
    writeln!(h).unwrap();
    writeln!(h, "#include <pb_decode.h>").unwrap();
    writeln!(h, "#include <pb_encode.h>").unwrap();
    writeln!(h).unwrap();
    writeln!(h, "#include <grpcuds.h>").unwrap();
    writeln!(h).unwrap();
    writeln!(h, "#include \"{base}.pb.h\"").unwrap();
    writeln!(h).unwrap();
    writeln!(
        h,
        "// On-stack scratch for one encoded message. To raise it, define it"
    )
    .unwrap();
    writeln!(
        h,
        "// PROJECT-WIDE (-DGRPCUDSC_MAX_MESSAGE_SIZE=...): sync unary responses"
    )
    .unwrap();
    writeln!(
        h,
        "// are encoded inside the generated .c, so a per-TU override in your"
    )
    .unwrap();
    writeln!(h, "// code alone would silently not apply there.").unwrap();
    writeln!(h, "#ifndef GRPCUDSC_MAX_MESSAGE_SIZE").unwrap();
    writeln!(h, "#define GRPCUDSC_MAX_MESSAGE_SIZE 1024").unwrap();
    writeln!(h, "#endif").unwrap();
    writeln!(h).unwrap();
    writeln!(h, "#ifdef __cplusplus").unwrap();
    writeln!(h, "extern \"C\" {{").unwrap();
    writeln!(h, "#endif").unwrap();
    writeln!(h).unwrap();

    let mut s = String::new();
    writeln!(
        s,
        "// Generated by protoc-gen-grpcudspp (C mode) from {name}. DO NOT EDIT."
    )
    .unwrap();
    writeln!(s, "//").unwrap();
    writeln!(
        s,
        "// Server trampolines + registration. Compile into SERVER binaries only"
    )
    .unwrap();
    writeln!(
        s,
        "// (the client-side wrappers are static inline in {header_name})."
    )
    .unwrap();
    writeln!(s, "#include \"{header_name}\"").unwrap();
    writeln!(s).unwrap();

    for svc in &proto.service {
        write_c_service(&mut h, &mut s, svc, pkg)?;
    }

    writeln!(h, "#ifdef __cplusplus").unwrap();
    writeln!(h, "}}  // extern \"C\"").unwrap();
    writeln!(h, "#endif").unwrap();
    writeln!(h).unwrap();
    writeln!(h, "#endif  // {guard}").unwrap();

    let mut h_file = ResponseFile::new();
    h_file.set_name(header_name);
    h_file.set_content(h);
    let mut s_file = ResponseFile::new();
    s_file.set_name(source_name);
    s_file.set_content(s);
    Ok((h_file, s_file))
}

/// nanopb flat C type for a `.foo.bar.Baz` reference — like
/// [`qualified_type`] but without the C++ `::` prefix.
fn c_type(proto_ref: &str) -> String {
    proto_ref.trim_start_matches('.').replace('.', "_")
}

fn write_c_service(
    h: &mut String,
    s: &mut String,
    svc: &ServiceDescriptorProto,
    pkg: &str,
) -> Result<(), String> {
    let svc_name = svc.name();
    let full = if pkg.is_empty() {
        svc_name.to_string()
    } else {
        format!("{pkg}.{svc_name}")
    };
    let prefix = full.replace('.', "_");

    // C identifiers the generated surface cannot tolerate as method names:
    // C keywords (a struct field named `register` is a syntax error) plus
    // the two names the generator itself occupies in the table/prefix
    // namespace. The C++ mode is untouched by this — C++ allows most of
    // these as members.
    const C_RESERVED: &[&str] = &[
        "auto",
        "break",
        "case",
        "char",
        "const",
        "continue",
        "default",
        "do",
        "double",
        "else",
        "enum",
        "extern",
        "float",
        "for",
        "goto",
        "if",
        "inline",
        "int",
        "long",
        "register",
        "restrict",
        "return",
        "short",
        "signed",
        "sizeof",
        "static",
        "struct",
        "switch",
        "typedef",
        "union",
        "unsigned",
        "void",
        "volatile",
        "while",
        // C99/C11 underscore keywords (visible via the generated header's
        // <pb.h>/<stdint.h> includes).
        "_Bool",
        "_Atomic",
        "_Complex",
        "_Imaginary",
        "_Generic",
        "_Noreturn",
        "_Static_assert",
        "_Thread_local",
        "_Alignas",
        "_Alignof",
        // Standard macros in scope (stdbool.h via nanopb, stddef.h).
        "bool",
        "true",
        "false",
        "NULL",
        // Generator-occupied identifiers in the table/prefix namespace.
        "user_data",
        "service",
    ];
    for m in &svc.method {
        if m.client_streaming() {
            return Err(format!(
                "method {}: client-streaming RPCs are not supported",
                m.name()
            ));
        }
        // Leading-underscore identifiers are reserved to the C
        // implementation; reject rather than emit reserved-namespace code.
        if C_RESERVED.contains(&m.name()) || m.name().starts_with('_') {
            return Err(format!(
                "method {}: the name is reserved in the generated C surface \
                 (C keyword, standard macro, leading underscore, or \
                 generator-occupied identifier)",
                m.name()
            ));
        }
    }

    // ---- header: the service table ------------------------------------
    writeln!(
        h,
        "/* ==== {full} : server ==================================== */"
    )
    .unwrap();
    writeln!(h).unwrap();
    writeln!(h, "typedef struct {prefix}_service {{").unwrap();
    writeln!(h, "    /* Handed back to every handler below. */").unwrap();
    writeln!(h, "    void* user_data;").unwrap();
    for m in &svc.method {
        let name = m.name();
        let req = c_type(m.input_type());
        let resp = c_type(m.output_type());
        writeln!(h).unwrap();
        if m.server_streaming() {
            writeln!(h, "    /* Server-streaming {name}: push messages with").unwrap();
            writeln!(
                h,
                "     * {prefix}_{name}_send() (I/O thread; see docs/THREADING.md for"
            )
            .unwrap();
            writeln!(
                h,
                "     * producer threads), end with {prefix}_{name}_finish(). Return"
            )
            .unwrap();
            writeln!(
                h,
                "     * GRPCUDS_OK to keep the stream open; any other status finishes"
            )
            .unwrap();
            writeln!(
                h,
                "     * the call with it. NULL = method not registered. */"
            )
            .unwrap();
            writeln!(
                h,
                "    int (*{name})(grpcuds_call_ref ref, const {req}* req,"
            )
            .unwrap();
            writeln!(h, "                  void* user_data);").unwrap();
        } else {
            writeln!(
                h,
                "    /* Unary {name}: fill `resp`, return GRPCUDS_OK. Any other gRPC"
            )
            .unwrap();
            writeln!(
                h,
                "     * status finishes the call with that status instead. Return"
            )
            .unwrap();
            writeln!(
                h,
                "     * GRPCUDS_HANDLER_DEFERRED to keep the call open and complete it"
            )
            .unwrap();
            writeln!(
                h,
                "     * later with {prefix}_{name}_respond() or grpcuds_call_finish()."
            )
            .unwrap();
            writeln!(h, "     * NULL = method not registered. */").unwrap();
            writeln!(
                h,
                "    int (*{name})(grpcuds_call_ref ref, const {req}* req, {resp}* resp,"
            )
            .unwrap();
            writeln!(h, "                  void* user_data);").unwrap();
        }
    }
    writeln!(h, "}} {prefix}_service;").unwrap();
    writeln!(h).unwrap();
    writeln!(
        h,
        "/* Register every non-NULL handler on `s`. `svc` must outlive the server"
    )
    .unwrap();
    writeln!(
        h,
        " * (a static or main-scope struct). 0, or the first register failure. */"
    )
    .unwrap();
    writeln!(
        h,
        "int {prefix}_register(grpcuds_server* s, const {prefix}_service* svc);"
    )
    .unwrap();
    writeln!(h).unwrap();

    // ---- header: server-side helpers (static inline) --------------------
    for m in &svc.method {
        let name = m.name();
        let resp = c_type(m.output_type());
        if m.server_streaming() {
            writeln!(
                h,
                "/* Encode + queue one {resp} on the open {name} stream. */"
            )
            .unwrap();
            writeln!(
                h,
                "static inline int {prefix}_{name}_send(grpcuds_call_ref ref,"
            )
            .unwrap();
            writeln!(
                h,
                "                                       const {resp}* msg) {{"
            )
            .unwrap();
            writeln!(h, "    uint8_t buf[GRPCUDSC_MAX_MESSAGE_SIZE];").unwrap();
            writeln!(
                h,
                "    pb_ostream_t os = pb_ostream_from_buffer(buf, sizeof(buf));"
            )
            .unwrap();
            writeln!(
                h,
                "    if (!pb_encode(&os, {resp}_fields, msg)) return GRPCUDS_ERR_CODEC;"
            )
            .unwrap();
            writeln!(
                h,
                "    return grpcuds_call_write(ref.call, ref.call_id, buf, os.bytes_written);"
            )
            .unwrap();
            writeln!(h, "}}").unwrap();
            writeln!(h).unwrap();
            writeln!(h, "/* End the {name} stream with a gRPC status. */").unwrap();
            writeln!(
                h,
                "static inline int {prefix}_{name}_finish(grpcuds_call_ref ref, int status) {{"
            )
            .unwrap();
            writeln!(
                h,
                "    return grpcuds_call_finish(ref.call, ref.call_id, status);"
            )
            .unwrap();
            writeln!(h, "}}").unwrap();
            writeln!(h).unwrap();
        } else {
            writeln!(
                h,
                "/* Complete a deferred unary {name}: encode + write + finish OK. */"
            )
            .unwrap();
            writeln!(
                h,
                "static inline int {prefix}_{name}_respond(grpcuds_call_ref ref,"
            )
            .unwrap();
            writeln!(
                h,
                "                                          const {resp}* resp) {{"
            )
            .unwrap();
            writeln!(h, "    uint8_t buf[GRPCUDSC_MAX_MESSAGE_SIZE];").unwrap();
            writeln!(
                h,
                "    pb_ostream_t os = pb_ostream_from_buffer(buf, sizeof(buf));"
            )
            .unwrap();
            writeln!(
                h,
                "    if (!pb_encode(&os, {resp}_fields, resp)) return GRPCUDS_ERR_CODEC;"
            )
            .unwrap();
            writeln!(
                h,
                "    int rc = grpcuds_call_write(ref.call, ref.call_id, buf, os.bytes_written);"
            )
            .unwrap();
            writeln!(h, "    if (rc != 0) return rc;").unwrap();
            writeln!(
                h,
                "    return grpcuds_call_finish(ref.call, ref.call_id, GRPCUDS_OK);"
            )
            .unwrap();
            writeln!(h, "}}").unwrap();
            writeln!(h).unwrap();
        }
    }

    // ---- header: client wrappers (static inline) ------------------------
    writeln!(
        h,
        "/* ==== {full} : client ==================================== */"
    )
    .unwrap();
    writeln!(h).unwrap();
    for m in &svc.method {
        let name = m.name();
        let req = c_type(m.input_type());
        let resp = c_type(m.output_type());
        let path = format!("/{full}/{name}");
        if m.server_streaming() {
            writeln!(
                h,
                "/* Open the {name} stream. NULL on encode/transport failure. */"
            )
            .unwrap();
            writeln!(
                h,
                "static inline grpcuds_stream* {prefix}_{name}_start(grpcuds_client* c,"
            )
            .unwrap();
            writeln!(
                h,
                "                                                    const {req}* req) {{"
            )
            .unwrap();
            writeln!(h, "    uint8_t buf[GRPCUDSC_MAX_MESSAGE_SIZE];").unwrap();
            writeln!(
                h,
                "    pb_ostream_t os = pb_ostream_from_buffer(buf, sizeof(buf));"
            )
            .unwrap();
            writeln!(
                h,
                "    if (!pb_encode(&os, {req}_fields, req)) return NULL;"
            )
            .unwrap();
            writeln!(
                h,
                "    return grpcuds_client_server_streaming(c, \"{path}\", buf, os.bytes_written);"
            )
            .unwrap();
            writeln!(h, "}}").unwrap();
            writeln!(h).unwrap();
            writeln!(
                h,
                "/* 1 = a message was decoded into `out`; 0 = stream end (check"
            )
            .unwrap();
            writeln!(
                h,
                " * grpcuds_stream_status); GRPCUDS_ERR_CODEC on a decode failure. */"
            )
            .unwrap();
            writeln!(
                h,
                "static inline int {prefix}_{name}_next(grpcuds_stream* s, {resp}* out) {{"
            )
            .unwrap();
            writeln!(h, "    size_t len = 0;").unwrap();
            writeln!(
                h,
                "    const uint8_t* bytes = grpcuds_stream_next(s, &len);"
            )
            .unwrap();
            writeln!(h, "    if (!bytes) return 0;").unwrap();
            writeln!(h, "    {resp} zero = {resp}_init_zero;").unwrap();
            writeln!(h, "    *out = zero;").unwrap();
            writeln!(
                h,
                "    pb_istream_t is = pb_istream_from_buffer(bytes, len);"
            )
            .unwrap();
            writeln!(
                h,
                "    return pb_decode(&is, {resp}_fields, out) ? 1 : GRPCUDS_ERR_CODEC;"
            )
            .unwrap();
            writeln!(h, "}}").unwrap();
            writeln!(h).unwrap();
        } else {
            writeln!(
                h,
                "/* Unary {name}. Returns the gRPC status (0 = OK, `resp` filled),"
            )
            .unwrap();
            writeln!(h, " * GRPCUDS_ERR_TRANSPORT, or GRPCUDS_ERR_CODEC. */").unwrap();
            writeln!(
                h,
                "static inline int {prefix}_{name}(grpcuds_client* c, const {req}* req,"
            )
            .unwrap();
            writeln!(h, "                                  {resp}* resp) {{").unwrap();
            writeln!(h, "    uint8_t buf[GRPCUDSC_MAX_MESSAGE_SIZE];").unwrap();
            writeln!(
                h,
                "    pb_ostream_t os = pb_ostream_from_buffer(buf, sizeof(buf));"
            )
            .unwrap();
            writeln!(
                h,
                "    if (!pb_encode(&os, {req}_fields, req)) return GRPCUDS_ERR_CODEC;"
            )
            .unwrap();
            writeln!(h, "    grpcuds_response* r = grpcuds_client_unary(c, \"{path}\", buf, os.bytes_written);").unwrap();
            writeln!(h, "    if (!r) return GRPCUDS_ERR_TRANSPORT;").unwrap();
            writeln!(h, "    int status = grpcuds_response_status(r);").unwrap();
            writeln!(h, "    if (status == GRPCUDS_OK) {{").unwrap();
            writeln!(h, "        size_t len = 0;").unwrap();
            writeln!(
                h,
                "        const uint8_t* body = grpcuds_response_body(r, &len);"
            )
            .unwrap();
            writeln!(h, "        {resp} zero = {resp}_init_zero;").unwrap();
            writeln!(h, "        *resp = zero;").unwrap();
            writeln!(
                h,
                "        pb_istream_t is = pb_istream_from_buffer(body, len);"
            )
            .unwrap();
            writeln!(
                h,
                "        if (!pb_decode(&is, {resp}_fields, resp)) status = GRPCUDS_ERR_CODEC;"
            )
            .unwrap();
            writeln!(h, "    }}").unwrap();
            writeln!(h, "    grpcuds_response_free(r);").unwrap();
            writeln!(h, "    return status;").unwrap();
            writeln!(h, "}}").unwrap();
            writeln!(h).unwrap();
        }
    }

    // ---- source: trampolines + registration ------------------------------
    for m in &svc.method {
        let name = m.name();
        let req = c_type(m.input_type());
        let resp = c_type(m.output_type());
        writeln!(
            s,
            "static int {prefix}_{name}_tr(void* call, int32_t call_id,"
        )
        .unwrap();
        writeln!(
            s,
            "                              const uint8_t* req_bytes, size_t req_len,"
        )
        .unwrap();
        writeln!(s, "                              void* user_data) {{").unwrap();
        writeln!(
            s,
            "    const {prefix}_service* svc = (const {prefix}_service*)user_data;"
        )
        .unwrap();
        writeln!(s, "    {req} request = {req}_init_zero;").unwrap();
        writeln!(
            s,
            "    pb_istream_t is = pb_istream_from_buffer(req_bytes, req_len);"
        )
        .unwrap();
        writeln!(s, "    if (!pb_decode(&is, {req}_fields, &request)) {{").unwrap();
        writeln!(s, "        return GRPCUDS_INVALID_ARGUMENT;").unwrap();
        writeln!(s, "    }}").unwrap();
        writeln!(s, "    grpcuds_call_ref ref;").unwrap();
        writeln!(s, "    ref.call = call;").unwrap();
        writeln!(s, "    ref.call_id = call_id;").unwrap();
        if m.server_streaming() {
            writeln!(s, "    return svc->{name}(ref, &request, svc->user_data);").unwrap();
        } else {
            writeln!(s, "    {resp} response = {resp}_init_zero;").unwrap();
            writeln!(
                s,
                "    int rc = svc->{name}(ref, &request, &response, svc->user_data);"
            )
            .unwrap();
            writeln!(
                s,
                "    if (rc == GRPCUDS_HANDLER_DEFERRED) return GRPCUDS_OK;"
            )
            .unwrap();
            writeln!(s, "    if (rc != GRPCUDS_OK) return rc;").unwrap();
            writeln!(s, "    return {prefix}_{name}_respond(ref, &response) == 0").unwrap();
            writeln!(s, "               ? GRPCUDS_OK").unwrap();
            writeln!(s, "               : GRPCUDS_INTERNAL;").unwrap();
        }
        writeln!(s, "}}").unwrap();
        writeln!(s).unwrap();
    }
    writeln!(
        s,
        "int {prefix}_register(grpcuds_server* s, const {prefix}_service* svc) {{"
    )
    .unwrap();
    writeln!(s, "    int rc = 0;").unwrap();
    writeln!(s, "    (void)rc;").unwrap();
    for m in &svc.method {
        let name = m.name();
        let path = format!("/{full}/{name}");
        writeln!(s, "    if (svc->{name}) {{").unwrap();
        writeln!(
            s,
            "        rc = grpcuds_server_register_method(s, \"{path}\","
        )
        .unwrap();
        writeln!(
            s,
            "                                            {prefix}_{name}_tr, (void*)svc);"
        )
        .unwrap();
        writeln!(s, "        if (rc != 0) return rc;").unwrap();
        writeln!(s, "    }}").unwrap();
    }
    writeln!(s, "    return 0;").unwrap();
    writeln!(s, "}}").unwrap();
    writeln!(s).unwrap();
    Ok(())
}

/// Unique response types, in declaration order, for one streaming flavor —
/// specialization emitters must not repeat a type (ODR).
fn collect_resp_types(
    services: &[ServiceDescriptorProto],
    pkg: &str,
    server_streaming: bool,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for svc in services {
        for method in &svc.method {
            if method.server_streaming() == server_streaming {
                let resp = qualified_type(method.output_type(), pkg);
                if !out.contains(&resp) {
                    out.push(resp);
                }
            }
        }
    }
    out
}

fn write_header_prologue(
    out: &mut String,
    header_name: &str,
    nanopb_header: &str,
    proto_source: &str,
    need_cstring: bool,
) -> Result<(), String> {
    let guard = guard_for(header_name);
    writeln!(out, "// Generated by protoc-gen-grpcudspp. DO NOT EDIT.").unwrap();
    writeln!(out, "// source: {}", proto_source).unwrap();
    writeln!(out, "#ifndef {}", guard).unwrap();
    writeln!(out, "#define {}", guard).unwrap();
    writeln!(out).unwrap();
    if need_cstring {
        // The chainable <Msg>Mut field setters use std::strncpy / std::memcpy
        // (string / bytes fields) and std::size_t.
        writeln!(out, "#include <cstddef>").unwrap();
        writeln!(out, "#include <cstring>").unwrap();
    }
    writeln!(out, "#include <memory>").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "#include <grpcudspp/grpcudspp.h>").unwrap();
    // The typed client Stub. client.h's typed API (and the Stub below) only
    // compiles when nanopb is on the include path; pure server builds still
    // work because the Stub is behind GRPCUDSPP_HAVE_NANOPB and its methods
    // only reference the client C ABI when actually used.
    writeln!(out, "#include <grpcudspp/client.h>").unwrap();
    writeln!(out, "#include \"{}\"", nanopb_header).unwrap();
    writeln!(out).unwrap();
    Ok(())
}

fn write_header_epilogue(out: &mut String, header_name: &str) {
    let guard = guard_for(header_name);
    writeln!(out, "#endif  // {}", guard).unwrap();
}

fn write_source_prologue(
    out: &mut String,
    header_name: &str,
    proto_source: &str,
) -> Result<(), String> {
    writeln!(out, "// Generated by protoc-gen-grpcudspp. DO NOT EDIT.").unwrap();
    writeln!(out, "// source: {}", proto_source).unwrap();
    writeln!(out, "#include \"{}\"", header_name).unwrap();
    writeln!(out).unwrap();
    writeln!(out, "#include <pb_decode.h>").unwrap();
    writeln!(out, "#include <pb_encode.h>").unwrap();
    writeln!(out).unwrap();
    // Emit a comment block above the size define so a developer reading
    // the generated source learns where the 1024 came from and how to
    // raise it. Mirrored in docs/MIGRATING_FROM_GRPC_CPP.md and the
    // top-level README's "Quick build" section.
    writeln!(
        out,
        "// Stack buffer size for per-call encode / decode. 1024 B covers the typical"
    )
    .unwrap();
    writeln!(
        out,
        "// BLE control / event payload while keeping each trampoline's stack frame"
    )
    .unwrap();
    writeln!(
        out,
        "// cheap on an embedded device. If a handler legitimately needs larger messages"
    )
    .unwrap();
    writeln!(
        out,
        "// (GATT blob write, 4 KB advertising-data dump, …) override at compile time:"
    )
    .unwrap();
    writeln!(out, "//").unwrap();
    writeln!(out, "//     // CMake").unwrap();
    writeln!(
        out,
        "//     target_compile_definitions(my-app PRIVATE GRPCUDSPP_MAX_MESSAGE_SIZE=4096)"
    )
    .unwrap();
    writeln!(out, "//").unwrap();
    writeln!(out, "//     // bare cc").unwrap();
    writeln!(out, "//     -DGRPCUDSPP_MAX_MESSAGE_SIZE=4096").unwrap();
    writeln!(out, "//").unwrap();
    writeln!(
        out,
        "// The override must be supplied to *every* translation unit that includes a"
    )
    .unwrap();
    writeln!(
        out,
        "// generated `*.grpc.pb.cc` or you'll end up with inconsistent buffer sizes"
    )
    .unwrap();
    writeln!(
        out,
        "// across trampolines. See docs/MIGRATING_FROM_GRPC_CPP.md (\"Variable-size"
    )
    .unwrap();
    writeln!(out, "// fields\") for the longer rationale.").unwrap();
    writeln!(out, "#ifndef GRPCUDSPP_MAX_MESSAGE_SIZE").unwrap();
    writeln!(out, "#define GRPCUDSPP_MAX_MESSAGE_SIZE 1024").unwrap();
    writeln!(out, "#endif").unwrap();
    writeln!(out).unwrap();
    Ok(())
}

fn write_service_header(
    out: &mut String,
    svc: &ServiceDescriptorProto,
    pkg: &str,
) -> Result<(), String> {
    let svc_name = svc.name();
    writeln!(out, "class {} final {{", svc_name).unwrap();
    writeln!(out, " public:").unwrap();
    writeln!(out, "    static const char* service_full_name() {{").unwrap();
    let full = if pkg.is_empty() {
        svc_name.to_string()
    } else {
        format!("{}.{}", pkg, svc_name)
    };
    writeln!(out, "        return \"{}\";", full).unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "    class Service : public grpcuds::Service {{").unwrap();
    writeln!(out, "     public:").unwrap();
    writeln!(out, "        ~Service() override = default;").unwrap();
    writeln!(out).unwrap();

    for method in &svc.method {
        write_method_decl(out, method, pkg)?;
    }

    writeln!(
        out,
        "        void BindToServer(grpcuds_server* server) override;"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "     private:").unwrap();
    for method in &svc.method {
        let trampoline_name = format!("{}Trampoline", method.name());
        writeln!(
            out,
            "        static int {}(void* call, int32_t call_id,\n                                       const uint8_t* req_bytes, size_t req_len,\n                                       void* user_data);",
            trampoline_name
        )
        .unwrap();
    }
    writeln!(out, "    }};").unwrap();
    writeln!(out).unwrap();

    write_stub_header(out, svc, pkg)?;

    writeln!(out, "}};").unwrap();
    writeln!(out).unwrap();
    Ok(())
}

/// The typed client stub — the stock-gRPC `NewStub` shape over the blocking
/// `grpcuds::Client`. Method paths + nanopb descriptors are baked in, so call
/// sites are `stub->Say(req, &reply)` instead of the path-and-fields API.
fn write_stub_header(
    out: &mut String,
    svc: &ServiceDescriptorProto,
    pkg: &str,
) -> Result<(), String> {
    let svc_name = svc.name();
    let full = if pkg.is_empty() {
        svc_name.to_string()
    } else {
        format!("{}.{}", pkg, svc_name)
    };

    writeln!(out, "#ifdef GRPCUDSPP_HAVE_NANOPB").unwrap();
    writeln!(
        out,
        "    // Typed client stub over a connected grpcuds::Client. The Client must"
    )
    .unwrap();
    writeln!(
        out,
        "    // outlive the Stub; calls are blocking, one in flight at a time."
    )
    .unwrap();
    writeln!(out, "    class Stub final {{").unwrap();
    writeln!(out, "     public:").unwrap();
    writeln!(
        out,
        "        explicit Stub(grpcuds::Client& client) : client_(client) {{}}"
    )
    .unwrap();
    writeln!(out).unwrap();

    for method in &svc.method {
        let name = method.name();
        let path = format!("/{full}/{name}");
        let req = qualified_type(method.input_type(), pkg);
        let resp = qualified_type(method.output_type(), pkg);
        let req_fields = format!("{}_fields", req.trim_start_matches(':'));
        let resp_fields = format!("{}_fields", resp.trim_start_matches(':'));

        if method.server_streaming() {
            writeln!(
                out,
                "        grpcuds::ClientReader<{resp}> {name}(const {req}& request) {{"
            )
            .unwrap();
            writeln!(
                out,
                "            return client_.ServerStreaming<{req}, {resp}>("
            )
            .unwrap();
            writeln!(
                out,
                "                \"{path}\", request, {req_fields}, {resp_fields});"
            )
            .unwrap();
            writeln!(out, "        }}").unwrap();
        } else {
            writeln!(
                out,
                "        grpcuds::Status {name}(const {req}& request, {resp}* response) {{"
            )
            .unwrap();
            writeln!(
                out,
                "            return client_.Unary(\"{path}\", request, {req_fields},"
            )
            .unwrap();
            writeln!(
                out,
                "                                 response, {resp_fields});"
            )
            .unwrap();
            writeln!(out, "        }}").unwrap();
        }
        writeln!(out).unwrap();
    }

    writeln!(out, "     private:").unwrap();
    writeln!(out, "        grpcuds::Client& client_;").unwrap();
    writeln!(out, "    }};").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "    static std::unique_ptr<Stub> NewStub(grpcuds::Client& client) {{"
    )
    .unwrap();
    writeln!(
        out,
        "        return std::unique_ptr<Stub>(new Stub(client));"
    )
    .unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "#endif  // GRPCUDSPP_HAVE_NANOPB").unwrap();
    Ok(())
}

fn write_method_decl(
    out: &mut String,
    method: &MethodDescriptorProto,
    pkg: &str,
) -> Result<(), String> {
    let name = method.name();
    let req = qualified_type(method.input_type(), pkg);
    let resp = qualified_type(method.output_type(), pkg);

    // Reject features we don't support yet.
    if method.client_streaming() {
        return Err(format!(
            "method {name}: client-streaming RPCs are not supported"
        ));
    }

    if method.server_streaming() {
        // server-streaming: handler gets a ServerWriter<Resp>* and is
        // responsible for calling Write/Finish on it (sync or async).
        writeln!(out, "        virtual grpcuds::Status {}(", name).unwrap();
        writeln!(out, "            grpcuds::ServerContext* context,").unwrap();
        writeln!(out, "            const {}* request,", req).unwrap();
        writeln!(
            out,
            "            grpcuds::ServerWriter<{}>* writer) {{",
            resp
        )
        .unwrap();
    } else {
        // unary: handler fills in `response` and returns Status.
        writeln!(out, "        virtual grpcuds::Status {}(", name).unwrap();
        writeln!(out, "            grpcuds::ServerContext* context,").unwrap();
        writeln!(out, "            const {}* request,", req).unwrap();
        writeln!(out, "            {}* response) {{", resp).unwrap();
    }
    writeln!(out, "            (void)context;").unwrap();
    writeln!(out, "            (void)request;").unwrap();
    writeln!(
        out,
        "            {}",
        if method.server_streaming() {
            "(void)writer;"
        } else {
            "(void)response;"
        }
    )
    .unwrap();
    writeln!(
        out,
        "            return grpcuds::Status(grpcuds::UNIMPLEMENTED);"
    )
    .unwrap();
    writeln!(out, "        }}").unwrap();
    writeln!(out).unwrap();

    if !method.server_streaming() {
        // Deferred unary variant — the trampoline calls THIS overload. The
        // default delegates to the synchronous handler above and completes
        // immediately, so existing services are unchanged; overriding it
        // instead keeps the call open past the handler's return, to be
        // completed later from any thread via the responder (grpc++'s
        // callback-API ServerUnaryReactor shape).
        let resp_sym = resp.trim_start_matches("::");
        writeln!(
            out,
            "        // Deferred variant: override INSTEAD of the synchronous {} above",
            name
        )
        .unwrap();
        writeln!(
            out,
            "        // to complete the call later (any thread) via `responder`."
        )
        .unwrap();
        writeln!(out, "        virtual void {}(", name).unwrap();
        writeln!(out, "            grpcuds::ServerContext* context,").unwrap();
        writeln!(out, "            const {}* request,", req).unwrap();
        writeln!(
            out,
            "            grpcuds::UnaryResponder<{}> responder) {{",
            resp
        )
        .unwrap();
        writeln!(
            out,
            "            {} response = {}_init_zero;",
            resp, resp_sym
        )
        .unwrap();
        writeln!(
            out,
            "            grpcuds::Status status = {}(context, request, &response);",
            name
        )
        .unwrap();
        writeln!(out, "            if (status.ok()) {{").unwrap();
        writeln!(out, "                responder.Respond(response);").unwrap();
        writeln!(out, "            }} else {{").unwrap();
        writeln!(out, "                responder.Fail(status);").unwrap();
        writeln!(out, "            }}").unwrap();
        writeln!(out, "        }}").unwrap();
        writeln!(out).unwrap();
    }
    Ok(())
}

fn write_service_source(
    out: &mut String,
    svc: &ServiceDescriptorProto,
    pkg: &str,
) -> Result<(), String> {
    let svc_name = svc.name();
    let full = if pkg.is_empty() {
        svc_name.to_string()
    } else {
        format!("{}.{}", pkg, svc_name)
    };

    writeln!(
        out,
        "void {}::Service::BindToServer(grpcuds_server* server) {{",
        svc_name
    )
    .unwrap();
    for method in &svc.method {
        let path = format!("/{full}/{}", method.name());
        writeln!(
            out,
            "    grpcuds_server_register_method(server, \"{}\", &Service::{}Trampoline, this);",
            path,
            method.name()
        )
        .unwrap();
    }
    writeln!(out, "}}").unwrap();
    writeln!(out).unwrap();

    for method in &svc.method {
        write_method_trampoline(out, svc_name, method, pkg)?;
        writeln!(out).unwrap();
    }
    Ok(())
}

fn write_method_trampoline(
    out: &mut String,
    svc_name: &str,
    method: &MethodDescriptorProto,
    pkg: &str,
) -> Result<(), String> {
    let name = method.name();
    let req = qualified_type(method.input_type(), pkg);
    let resp = qualified_type(method.output_type(), pkg);
    // nanopb emits flat C symbols at global scope; we keep the same name
    // for `_fields` and `_init_zero` (no `::` prefix on these).
    let req_sym = req.trim_start_matches("::");
    let req_fields = format!("{}_fields", req_sym);
    let req_init = format!("{}_init_zero", req_sym);

    writeln!(
        out,
        "int {}::Service::{}Trampoline(void* call, int32_t call_id,",
        svc_name, name
    )
    .unwrap();
    writeln!(
        out,
        "                                       const uint8_t* req_bytes, size_t req_len,"
    )
    .unwrap();
    writeln!(
        out,
        "                                       void* user_data) {{"
    )
    .unwrap();
    writeln!(out, "    auto* self = static_cast<Service*>(user_data);").unwrap();
    writeln!(out, "    {} request = {};", req, req_init).unwrap();
    writeln!(
        out,
        "    pb_istream_t istream = pb_istream_from_buffer(req_bytes, req_len);"
    )
    .unwrap();
    writeln!(
        out,
        "    if (!pb_decode(&istream, {}, &request)) {{",
        req_fields
    )
    .unwrap();
    writeln!(
        out,
        "        grpcuds_call_finish(call, call_id, GRPCUDS_INVALID_ARGUMENT);"
    )
    .unwrap();
    writeln!(out, "        return GRPCUDS_INVALID_ARGUMENT;").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(out, "    grpcuds::ServerContext context(call, call_id);").unwrap();

    if method.server_streaming() {
        writeln!(
            out,
            "    grpcuds::ServerWriter<{}> writer(call, call_id);",
            resp
        )
        .unwrap();
        writeln!(
            out,
            "    grpcuds::Status status = self->{}(&context, &request, &writer);",
            name
        )
        .unwrap();
        writeln!(out, "    if (!status.ok()) {{").unwrap();
        writeln!(
            out,
            "        grpcuds_call_finish_msg(call, call_id, static_cast<int>(status.error_code()), reinterpret_cast<const uint8_t*>(status.error_message().data()), status.error_message().size());"
        )
        .unwrap();
        writeln!(out, "    }}").unwrap();
        // For OK return, the handler is responsible for calling
        // writer.Finish (possibly async). No auto-finish here.
        writeln!(out, "    return static_cast<int>(status.error_code());").unwrap();
    } else {
        // Unary completion is the responder's job: the default deferred
        // overload calls the synchronous handler and completes inline;
        // an overridden one completes later (any thread). Either way the
        // trampoline returns OK with the stream still owned by `responder`.
        writeln!(
            out,
            "    grpcuds::UnaryResponder<{}> responder(call, call_id);",
            resp
        )
        .unwrap();
        writeln!(out, "    self->{}(&context, &request, responder);", name).unwrap();
        writeln!(out, "    return GRPCUDS_OK;").unwrap();
    }
    writeln!(out, "}}").unwrap();
    Ok(())
}

fn write_server_writer_spec(out: &mut String, response_type: &str) {
    // response_type is already the absolute `::ble_*` form. Drop the
    // leading `::` for the nanopb `_fields` symbol at global scope.
    let resp_sym = response_type.trim_start_matches("::");
    let fields = format!("{}_fields", resp_sym);
    writeln!(out).unwrap();
    writeln!(
        out,
        "template <> bool ServerWriter<{}>::Write(const {}& message) {{",
        response_type, response_type
    )
    .unwrap();
    writeln!(out, "    uint8_t buf[GRPCUDSPP_MAX_MESSAGE_SIZE];").unwrap();
    writeln!(
        out,
        "    pb_ostream_t ostream = pb_ostream_from_buffer(buf, sizeof(buf));"
    )
    .unwrap();
    writeln!(
        out,
        "    if (!pb_encode(&ostream, {}, &message)) return false;",
        fields
    )
    .unwrap();
    writeln!(out, "    return raw_.Write(buf, ostream.bytes_written);").unwrap();
    writeln!(out, "}}").unwrap();
}

fn write_unary_responder_spec(out: &mut String, response_type: &str) {
    let resp_sym = response_type.trim_start_matches("::");
    let fields = format!("{}_fields", resp_sym);
    writeln!(out).unwrap();
    writeln!(
        out,
        "template <> bool UnaryResponder<{}>::Respond(const {}& response) {{",
        response_type, response_type
    )
    .unwrap();
    writeln!(out, "    uint8_t buf[GRPCUDSPP_MAX_MESSAGE_SIZE];").unwrap();
    writeln!(
        out,
        "    pb_ostream_t ostream = pb_ostream_from_buffer(buf, sizeof(buf));"
    )
    .unwrap();
    writeln!(
        out,
        "    if (!pb_encode(&ostream, {}, &response)) {{",
        fields
    )
    .unwrap();
    writeln!(
        out,
        "        raw_.Finish(Status(INTERNAL, \"response encode failed\"));"
    )
    .unwrap();
    writeln!(out, "        return false;").unwrap();
    writeln!(out, "    }}").unwrap();
    writeln!(
        out,
        "    if (!raw_.Write(buf, ostream.bytes_written)) return false;"
    )
    .unwrap();
    writeln!(out, "    return raw_.Finish(Status::Ok());").unwrap();
    writeln!(out, "}}").unwrap();
}

/// Emit a chainable `<Msg>Mut` ref-wrapper for one message and (recursively)
/// its nested messages. The wrapper holds a reference to the nanopb C struct
/// and exposes `set_<field>(...)` methods that return `*this`, so a migrated
/// handler can write `XMut(x).set_a(1).set_b("two")` in place of stock-gRPC's
/// `x.set_a(1); x.set_b("two");`.
///
/// nanopb structs are plain C — they cannot carry methods — so the wrapper is
/// a thin header-only adapter. Map-entry synthetic messages are skipped.
fn write_one_mutator(out: &mut String, pkg: &str, parents: &[String], desc: &DescriptorProto) {
    // Skip the synthetic map<> entry messages nanopb does not emit as structs.
    if desc.options.map_entry() {
        return;
    }

    let mut path: Vec<String> = parents.to_vec();
    path.push(desc.name().to_string());

    let wrapper = format!("{}Mut", path.join("_"));
    let c_type = message_c_type(pkg, &path);

    let mut body = String::new();
    for field in &desc.field {
        if let Some(setter) = field_setter(pkg, &wrapper, field) {
            body.push_str(&setter);
        }
    }

    // Only emit a wrapper when there is at least one settable field; an empty
    // wrapper would be dead weight.
    if !body.is_empty() {
        writeln!(out, "class {wrapper} {{").unwrap();
        writeln!(out, " public:").unwrap();
        writeln!(out, "    explicit {wrapper}({c_type}& m) : m_(m) {{}}").unwrap();
        out.push_str(&body);
        writeln!(out, " private:").unwrap();
        writeln!(out, "    {c_type}& m_;").unwrap();
        writeln!(out, "}};").unwrap();
        writeln!(out).unwrap();
    }

    // Recurse into nested message types.
    for nested in &desc.nested_type {
        write_one_mutator(out, pkg, &path, nested);
    }
}

/// One `set_<field>` method for the chainable wrapper, or `None` for fields we
/// can't safely set inline (message/group/repeated). String fields assume a
/// nanopb `max_size` (a fixed `char[]` buffer) per the project's `.options`
/// invariant; bytes fields assume a `PB_BYTES_ARRAY_T`.
fn field_setter(pkg: &str, wrapper: &str, field: &FieldDescriptorProto) -> Option<String> {
    use protobuf::descriptor::field_descriptor_proto::{Label, Type};

    if field.label() == Label::LABEL_REPEATED {
        return None;
    }

    let fname = field.name();
    let setter = format!("set_{}", fname);
    let mut out = String::new();

    match field.type_() {
        Type::TYPE_STRING => {
            // Bounds-safe copy into the fixed nanopb char[] buffer.
            writeln!(out, "    {wrapper}& {setter}(const char* v) {{").unwrap();
            writeln!(
                out,
                "        std::strncpy(m_.{fname}, v, sizeof(m_.{fname}) - 1);"
            )
            .unwrap();
            writeln!(out, "        m_.{fname}[sizeof(m_.{fname}) - 1] = '\\0';").unwrap();
            writeln!(out, "        return *this;").unwrap();
            writeln!(out, "    }}").unwrap();
        }
        Type::TYPE_BYTES => {
            writeln!(
                out,
                "    {wrapper}& {setter}(const void* p, std::size_t n) {{"
            )
            .unwrap();
            writeln!(out, "        std::size_t cap = sizeof(m_.{fname}.bytes);").unwrap();
            writeln!(out, "        if (n > cap) n = cap;").unwrap();
            writeln!(out, "        std::memcpy(m_.{fname}.bytes, p, n);").unwrap();
            writeln!(out, "        m_.{fname}.size = (pb_size_t)n;").unwrap();
            writeln!(out, "        return *this;").unwrap();
            writeln!(out, "    }}").unwrap();
        }
        Type::TYPE_MESSAGE | Type::TYPE_GROUP => return None,
        Type::TYPE_ENUM => {
            let ctype = qualified_type(field.type_name(), pkg);
            writeln!(
                out,
                "    {wrapper}& {setter}({ctype} v) {{ m_.{fname} = v; return *this; }}"
            )
            .unwrap();
        }
        other => {
            let ctype = scalar_c_type(other)?;
            writeln!(
                out,
                "    {wrapper}& {setter}({ctype} v) {{ m_.{fname} = v; return *this; }}"
            )
            .unwrap();
        }
    }
    Some(out)
}

/// C type for a proto scalar field, matching nanopb's struct field types.
fn scalar_c_type(ty: protobuf::descriptor::field_descriptor_proto::Type) -> Option<&'static str> {
    use protobuf::descriptor::field_descriptor_proto::Type;
    Some(match ty {
        Type::TYPE_BOOL => "bool",
        Type::TYPE_INT32 | Type::TYPE_SINT32 | Type::TYPE_SFIXED32 => "int32_t",
        Type::TYPE_UINT32 | Type::TYPE_FIXED32 => "uint32_t",
        Type::TYPE_INT64 | Type::TYPE_SINT64 | Type::TYPE_SFIXED64 => "int64_t",
        Type::TYPE_UINT64 | Type::TYPE_FIXED64 => "uint64_t",
        Type::TYPE_FLOAT => "float",
        Type::TYPE_DOUBLE => "double",
        _ => return None,
    })
}

/// Absolute nanopb C struct name for a message at `path` within `pkg`.
/// nanopb flattens the package + nested path with `_`, e.g. package
/// `ble` + path `[Container, Request]` → `::ble_Container_Request`.
fn message_c_type(pkg: &str, path: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !pkg.is_empty() {
        for p in pkg.split('.') {
            parts.push(p.to_string());
        }
    }
    for p in path {
        parts.push(p.clone());
    }
    format!("::{}", parts.join("_"))
}

/// Map a `.foo.bar.Baz` proto type reference to the nanopb-style C type
/// name. nanopb mangles every level of the proto path with `_`, so
/// `.ble.InitRequest` becomes `ble_InitRequest` and lives at global scope.
/// We emit absolute names (`::ble_InitRequest`) so the same source files
/// resolve correctly regardless of which `namespace` block they sit in.
fn qualified_type(proto_ref: &str, _pkg: &str) -> String {
    let flat = proto_ref.trim_start_matches('.').replace('.', "_");
    format!("::{flat}")
}

fn guard_for(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
        } else {
            out.push('_');
        }
    }
    out.push('_');
    out
}

// ---- tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::descriptor::{
        DescriptorProto, FieldDescriptorProto, FileDescriptorProto, MethodDescriptorProto,
        ServiceDescriptorProto,
    };

    fn make_request_proto() -> FileDescriptorProto {
        let mut file = FileDescriptorProto::new();
        file.set_name("ble.proto".to_string());
        file.set_package("ble".to_string());
        file.set_syntax("proto3".to_string());

        let mut svc = ServiceDescriptorProto::new();
        svc.set_name("BleService".to_string());

        // Unary: Init(InitRequest) → InitReply
        let mut init = MethodDescriptorProto::new();
        init.set_name("Init".to_string());
        init.set_input_type(".ble.InitRequest".to_string());
        init.set_output_type(".ble.InitReply".to_string());
        svc.method.push(init);

        // Server-stream: StartLeScan(ScanRequest) → stream ScanResult
        let mut scan = MethodDescriptorProto::new();
        scan.set_name("StartLeScan".to_string());
        scan.set_input_type(".ble.ScanRequest".to_string());
        scan.set_output_type(".ble.ScanResult".to_string());
        scan.set_server_streaming(true);
        svc.method.push(scan);

        file.service.push(svc);
        file
    }

    #[test]
    fn unary_and_streaming_codegen() {
        let file = make_request_proto();
        let (h, c) = generate_file(&file).unwrap();
        let header = h.content();
        let source = c.content();

        // Common scaffolding.
        assert!(header.contains("#include <grpcudspp/grpcudspp.h>"));
        assert!(header.contains("#include \"ble.pb.h\""));
        assert!(header.contains("namespace ble {"));
        assert!(header.contains("class BleService final"));
        assert!(header.contains("class Service : public grpcuds::Service"));

        // Unary virtual signature uses nanopb-style ::ble_* types.
        assert!(
            header.contains("virtual grpcuds::Status Init("),
            "missing unary method decl in header:\n{header}"
        );
        assert!(header.contains("const ::ble_InitRequest* request,"));
        assert!(header.contains("::ble_InitReply* response)"));

        // Deferred unary overload: responder-taking virtual whose default
        // delegates to the synchronous handler.
        assert!(
            header.contains("grpcuds::UnaryResponder<::ble_InitReply> responder)"),
            "missing deferred unary overload:\n{header}"
        );
        assert!(header.contains("::ble_InitReply response = ble_InitReply_init_zero;"));
        assert!(header.contains("responder.Respond(response);"));
        assert!(header.contains("responder.Fail(status);"));
        assert!(
            header.contains("template <> bool UnaryResponder<::ble_InitReply>::Respond("),
            "missing UnaryResponder specialization decl:\n{header}"
        );

        // Server-streaming virtual (writer instead of response).
        assert!(header.contains("virtual grpcuds::Status StartLeScan("));
        assert!(header.contains("grpcuds::ServerWriter<::ble_ScanResult>* writer)"));

        // ServerWriter<::ble_ScanResult>::Write declaration in grpcuds namespace.
        assert!(
            header.contains("template <> bool ServerWriter<::ble_ScanResult>::Write("),
            "missing ServerWriter specialization decl:\n{header}"
        );

        // Source: BindToServer with the right :path values.
        assert!(source.contains("void BleService::Service::BindToServer("));
        assert!(source.contains("\"/ble.BleService/Init\""));
        assert!(source.contains("\"/ble.BleService/StartLeScan\""));

        // Source: the unary trampoline hands the call to the responder and
        // returns OK — completion (sync default or deferred) is its job.
        assert!(source.contains("InitTrampoline"));
        assert!(source.contains("pb_decode(&istream, ble_InitRequest_fields,"));
        assert!(
            source.contains("grpcuds::UnaryResponder<::ble_InitReply> responder(call, call_id);")
        );
        assert!(source.contains("self->Init(&context, &request, responder);"));

        // UnaryResponder<::ble_InitReply>::Respond definition: encode +
        // write + Finish(OK), INTERNAL on encode failure.
        assert!(
            source.contains("template <> bool UnaryResponder<::ble_InitReply>::Respond("),
            "missing UnaryResponder specialization def:\n{source}"
        );
        assert!(source.contains("pb_encode(&ostream, ble_InitReply_fields,"));
        assert!(source.contains("raw_.Finish(Status::Ok());"));

        // Source: streaming trampoline does NOT auto-finish on OK.
        assert!(source.contains("StartLeScanTrampoline"));
        assert!(source.contains("grpcuds::ServerWriter<::ble_ScanResult> writer(call, call_id);"));

        // ServerWriter<::ble_ScanResult>::Write definition.
        assert!(
            source.contains("template <> bool ServerWriter<::ble_ScanResult>::Write("),
            "missing ServerWriter specialization def:\n{source}"
        );
        assert!(source.contains("pb_encode(&ostream, ble_ScanResult_fields,"));
    }

    /// The C mode (`--grpcudspp_opt=c`): same fixture, plain-C surface.
    #[test]
    fn c_mode_unary_and_streaming_codegen() {
        let file = make_request_proto();
        let (h_file, s_file) = generate_file_c(&file).expect("generate C");
        assert_eq!(h_file.name(), "ble.grpcuds.h");
        assert_eq!(s_file.name(), "ble.grpcuds.c");
        let header = h_file.content();
        let source = s_file.content();

        // Service table: user_data + one function pointer per RPC, flat
        // nanopb C types (no ::, no namespaces).
        assert!(
            header.contains("typedef struct ble_BleService_service {"),
            "missing service table:\n{header}"
        );
        assert!(header.contains(
            "int (*Init)(grpcuds_call_ref ref, const ble_InitRequest* req, ble_InitReply* resp,"
        ));
        assert!(
            header.contains("int (*StartLeScan)(grpcuds_call_ref ref, const ble_ScanRequest* req,")
        );
        assert!(header.contains(
            "int ble_BleService_register(grpcuds_server* s, const ble_BleService_service* svc);"
        ));

        // Server helpers are static inline IN THE HEADER (client-only
        // builds must not need the .c).
        assert!(
            header.contains("static inline int ble_BleService_Init_respond(grpcuds_call_ref ref,")
        );
        assert!(header
            .contains("static inline int ble_BleService_StartLeScan_send(grpcuds_call_ref ref,"));
        assert!(header.contains(
            "static inline int ble_BleService_StartLeScan_finish(grpcuds_call_ref ref, int status)"
        ));

        // Client wrappers, also static inline, with the right paths.
        assert!(header.contains(
            "static inline int ble_BleService_Init(grpcuds_client* c, const ble_InitRequest* req,"
        ));
        assert!(header.contains("\"/ble.BleService/Init\""));
        assert!(header.contains(
            "static inline grpcuds_stream* ble_BleService_StartLeScan_start(grpcuds_client* c,"
        ));
        assert!(header.contains("static inline int ble_BleService_StartLeScan_next(grpcuds_stream* s, ble_ScanResult* out)"));

        // Source: trampolines decode with nanopb, honor the deferred
        // sentinel, auto-finish unary; streaming returns the handler's
        // status verbatim (OK keeps the stream open). Registration skips
        // NULL handlers.
        assert!(source.contains("static int ble_BleService_Init_tr(void* call, int32_t call_id,"));
        assert!(source.contains("pb_decode(&is, ble_InitRequest_fields, &request)"));
        assert!(source.contains("if (rc == GRPCUDS_HANDLER_DEFERRED) return GRPCUDS_OK;"));
        assert!(source.contains("ble_BleService_Init_respond(ref, &response)"));
        assert!(source.contains("return svc->StartLeScan(ref, &request, svc->user_data);"));
        assert!(source.contains("if (svc->Init) {"));
        assert!(source.contains("\"/ble.BleService/StartLeScan\","));
    }

    /// Emitter edges: no package, two services in one file, nested message
    /// types, and two methods sharing a response type — the shapes most
    /// likely to break name construction.
    #[test]
    fn c_mode_edge_shapes() {
        let mut file = FileDescriptorProto::new();
        file.set_name("edge.proto".to_string());
        // NO package on purpose.
        let mut first = ServiceDescriptorProto::new();
        first.set_name("First".to_string());
        let mut echo = MethodDescriptorProto::new();
        echo.set_name("Echo".to_string());
        echo.set_input_type(".Ping".to_string());
        echo.set_output_type(".Ping".to_string()); // req == resp
        let mut inner = MethodDescriptorProto::new();
        inner.set_name("Nested".to_string());
        inner.set_input_type(".Ping.Inner".to_string()); // nested type
        inner.set_output_type(".Ping".to_string()); // shares resp with Echo
        first.method.push(echo);
        first.method.push(inner);
        let mut second = ServiceDescriptorProto::new();
        second.set_name("Second".to_string());
        let mut say = MethodDescriptorProto::new();
        say.set_name("Echo".to_string()); // same RPC name, other service
        say.set_input_type(".Pong".to_string());
        say.set_output_type(".Pong".to_string());
        second.method.push(say);
        file.service.push(first);
        file.service.push(second);

        let (h_file, s_file) = generate_file_c(&file).expect("edges generate");
        let header = h_file.content();
        let source = s_file.content();

        // No package: prefix is the bare service name; path has no pkg.
        assert!(header.contains("typedef struct First_service {"));
        assert!(header.contains("typedef struct Second_service {"));
        assert!(source.contains("\"/First/Echo\""));
        assert!(source.contains("\"/Second/Echo\""));

        // Nested type mangles flat (Ping.Inner -> Ping_Inner).
        assert!(header.contains("const Ping_Inner* req"));

        // Same response type on two methods: helpers are PER-METHOD, so
        // both exist with distinct names (no dedup needed, no collision).
        assert!(header.contains("static inline int First_Echo_respond("));
        assert!(header.contains("static inline int First_Nested_respond("));
        // Same RPC name across services: prefixes keep them distinct.
        assert!(header.contains("static inline int Second_Echo(grpcuds_client* c,"));
        assert!(source.contains("static int First_Echo_tr("));
        assert!(source.contains("static int Second_Echo_tr("));
    }

    /// proto2 descriptors flow through the C mode identically (the emitter
    /// never looks at syntax).
    #[test]
    fn c_mode_proto2_generates() {
        let mut file = make_request_proto();
        file.set_syntax("proto2".to_string());
        let (h2, _) = generate_file_c(&file).expect("proto2");
        let (h3, _) = generate_file_c(&make_request_proto()).expect("proto3");
        assert_eq!(h2.content(), h3.content());
    }

    /// Method names that would break the generated C (keywords, occupied
    /// identifiers) are rejected with a clear error instead of emitting
    /// uncompilable code.
    #[test]
    fn c_mode_rejects_reserved_method_names() {
        for bad in [
            "register",
            "user_data",
            "int",
            "struct",
            "service",
            // underscore keywords, stdbool/stddef macros, leading underscore
            "_Bool",
            "_Atomic",
            "bool",
            "true",
            "false",
            "NULL",
            "_private",
        ] {
            let mut file = make_request_proto();
            file.service[0].method[0].set_name(bad.to_string());
            let err = generate_file_c(&file).expect_err("must reject");
            assert!(err.contains("reserved"), "{bad}: {err}");
        }
        // The C++ mode is intentionally untouched by the C reservation list.
        let mut file = make_request_proto();
        file.service[0].method[0].set_name("register".to_string());
        assert!(generate_file(&file).is_ok());
    }

    #[test]
    fn c_mode_rejects_client_streaming() {
        let mut file = make_request_proto();
        file.service[0].method[0].set_client_streaming(true);
        let err = generate_file_c(&file).expect_err("must reject");
        assert!(err.contains("client-streaming"));
    }

    #[test]
    fn skip_files_without_services() {
        let mut file = FileDescriptorProto::new();
        file.set_name("messages_only.proto".to_string());
        // No services — generator should skip emitting any file.
        let result = generate_file(&file);
        // generate_file is called by main() only when services exist, so
        // this isn't quite the production path; but the generator itself
        // should still emit a header guard + empty body without error.
        assert!(result.is_ok());
    }

    #[test]
    fn nested_message_types_mangle_to_flat_nanopb_symbols() {
        // A method whose request/response are NESTED messages, i.e. in the
        // .proto:
        //     message Container {
        //         message Request  { ... }
        //         message Response { ... }
        //     }
        // The descriptor references them by full path
        // `.ble.Container.Request`. nanopb flattens *every* path segment
        // with `_`, so the generated stub must reference
        // `::ble_Container_Request` (and the global `ble_Container_Request_fields`
        // / `_init_zero` symbols). This pins our qualified_type mangling
        // against nanopb's so a nested I/O type links correctly — the only
        // way proto2/oneof/nested could break is a type-name mismatch here,
        // since the plugin never touches message *fields* (nanopb owns
        // encode/decode, including oneof unions and nested layouts).
        let mut file = FileDescriptorProto::new();
        file.set_name("nested.proto".to_string());
        file.set_package("ble".to_string());
        file.set_syntax("proto3".to_string());

        let mut svc = ServiceDescriptorProto::new();
        svc.set_name("NestedService".to_string());

        let mut m = MethodDescriptorProto::new();
        m.set_name("DoThing".to_string());
        m.set_input_type(".ble.Container.Request".to_string());
        m.set_output_type(".ble.Container.Response".to_string());
        svc.method.push(m);

        file.service.push(svc);

        let (h, c) = generate_file(&file).unwrap();
        let header = h.content();
        let source = c.content();

        // Header: nested types flattened to ::ble_Container_{Request,Response}.
        assert!(
            header.contains("const ::ble_Container_Request* request,"),
            "nested request type not flattened correctly:\n{header}"
        );
        assert!(
            header.contains("::ble_Container_Response* response)"),
            "nested response type not flattened correctly:\n{header}"
        );

        // Source: the nanopb _fields / _init_zero symbols must match
        // nanopb's flat global-scope mangling (no `::` on the bare symbol).
        assert!(
            source.contains("ble_Container_Request request = ble_Container_Request_init_zero;"),
            "nested request init mismatch:\n{source}"
        );
        assert!(
            source.contains("pb_decode(&istream, ble_Container_Request_fields,"),
            "nested request _fields mismatch:\n{source}"
        );
        // Response init lives in the header now (the deferred default
        // overload); the encode in the UnaryResponder specialization.
        assert!(
            header
                .contains("::ble_Container_Response response = ble_Container_Response_init_zero;"),
            "nested response init mismatch:\n{header}"
        );
        assert!(
            source.contains("pb_encode(&ostream, ble_Container_Response_fields,"),
            "nested response _fields mismatch:\n{source}"
        );
        assert!(
            source.contains(
                "bool UnaryResponder<::ble_Container_Response>::Respond(const ::ble_Container_Response& response)"
            ),
            "nested responder specialization mismatch:\n{source}"
        );
    }

    #[test]
    fn proto2_services_generate_identically() {
        // Service codegen is syntax-neutral. proto2 vs proto3 differ only
        // in field-presence / default-value / group semantics, all of
        // which live in the nanopb message layer the plugin delegates to.
        // A proto2 file with the same service shape must produce the same
        // service scaffolding without error — we must NOT reject proto2.
        let mut file = make_request_proto();
        file.set_syntax("proto2".to_string());

        let (h, c) = generate_file(&file).expect("proto2 service must generate");
        let header = h.content();
        let source = c.content();

        // Identical scaffolding to the proto3 path.
        assert!(header.contains("class BleService final"));
        assert!(header.contains("virtual grpcuds::Status Init("));
        assert!(header.contains("const ::ble_InitRequest* request,"));
        assert!(header.contains("grpcuds::ServerWriter<::ble_ScanResult>* writer)"));
        assert!(source.contains("\"/ble.BleService/Init\""));
    }

    #[test]
    fn generates_chainable_field_mutators() {
        use protobuf::descriptor::field_descriptor_proto::{Label, Type};

        let mut file = make_request_proto();

        // A message with one of each settable field kind plus an unsettable
        // (repeated) field, to pin the generated <Msg>Mut wrapper.
        let mut msg = DescriptorProto::new();
        msg.set_name("ScanResult".to_string());

        let mut addr = FieldDescriptorProto::new();
        addr.set_name("device_address".to_string());
        addr.set_type(Type::TYPE_STRING);
        addr.set_label(Label::LABEL_OPTIONAL);
        msg.field.push(addr);

        let mut rssi = FieldDescriptorProto::new();
        rssi.set_name("rssi".to_string());
        rssi.set_type(Type::TYPE_INT32);
        rssi.set_label(Label::LABEL_OPTIONAL);
        msg.field.push(rssi);

        let mut adv = FieldDescriptorProto::new();
        adv.set_name("adv_data".to_string());
        adv.set_type(Type::TYPE_BYTES);
        adv.set_label(Label::LABEL_OPTIONAL);
        msg.field.push(adv);

        let mut tags = FieldDescriptorProto::new();
        tags.set_name("tags".to_string());
        tags.set_type(Type::TYPE_STRING);
        tags.set_label(Label::LABEL_REPEATED);
        msg.field.push(tags);

        file.message_type.push(msg);

        let (h, _c) = generate_file(&file).unwrap();
        let header = h.content();

        // Wrapper lives inside the package namespace, named <Msg>Mut.
        assert!(
            header.contains("class ScanResultMut {"),
            "missing mutator wrapper:\n{header}"
        );
        assert!(header.contains("explicit ScanResultMut(::ble_ScanResult& m) : m_(m) {}"));

        // String setter is bounds-safe and chainable.
        assert!(header.contains("ScanResultMut& set_device_address(const char* v) {"));
        assert!(
            header.contains("std::strncpy(m_.device_address, v, sizeof(m_.device_address) - 1);")
        );

        // Scalar setter.
        assert!(
            header.contains("ScanResultMut& set_rssi(int32_t v) { m_.rssi = v; return *this; }")
        );

        // Bytes setter maps to .bytes / .size.
        assert!(header.contains("ScanResultMut& set_adv_data(const void* p, std::size_t n) {"));
        assert!(header.contains("m_.adv_data.size = (pb_size_t)n;"));

        // Repeated field has no setter.
        assert!(
            !header.contains("set_tags"),
            "repeated field must not get a setter:\n{header}"
        );

        // string/bytes setters pull in <cstring>.
        assert!(header.contains("#include <cstring>"));
    }

    #[test]
    fn rejects_client_streaming() {
        let mut file = make_request_proto();
        let mut bidi = MethodDescriptorProto::new();
        bidi.set_name("Chat".to_string());
        bidi.set_input_type(".ble.ChatMessage".to_string());
        bidi.set_output_type(".ble.ChatMessage".to_string());
        bidi.set_client_streaming(true);
        file.service[0].method.push(bidi);

        let err = generate_file(&file).err().unwrap();
        assert!(err.contains("client-streaming"), "got error: {err}");
    }
}
