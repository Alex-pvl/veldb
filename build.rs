fn main() {
    println!("cargo:rerun-if-changed=proto/veldb.proto");
    if let Err(e) = tonic_prost_build::compile_protos("proto/veldb.proto") {
        // Ошибка prost-build про отсутствующий protoc тонет в выводе cargo и не
        // говорит, что именно ставить на этой системе. Подменяем её на инструкцию.
        panic!("{}", explain(e));
    }
}

fn explain(e: std::io::Error) -> String {
    if e.kind() != std::io::ErrorKind::NotFound {
        return format!("генерация gRPC-кода из proto/veldb.proto: {e}");
    }
    concat!(
        "\n\n",
        "  Не найден protoc. Он нужен на этапе сборки, чтобы сгенерировать\n",
        "  gRPC-код из proto/veldb.proto.\n",
        "\n",
        "  Debian / Ubuntu / Kali / Raspberry Pi OS:\n",
        "      sudo apt update && sudo apt install -y protobuf-compiler\n",
        "\n",
        "  Fedora / RHEL:   sudo dnf install -y protobuf-compiler\n",
        "  Alpine:          apk add protoc\n",
        "  macOS:           brew install protobuf\n",
        "\n",
        "  Если protoc уже стоит, но не в PATH — укажите путь явно:\n",
        "      PROTOC=/путь/к/protoc cargo build --release\n"
    )
    .to_string()
}
