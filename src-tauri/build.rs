fn main() {
    let mut attrs = tauri_build::Attributes::new();

    // Windows: manifesto UAC (requestedExecutionLevel = requireAdministrator) para o .exe
    // já solicitar elevação ao abrir — requisito para acessar \\.\PhysicalDriveX.
    // Deve ser injetado via tauri-build (que embute o manifesto padrão): usar
    // embed-resource em paralelo gera "CVT1100 duplicate resource MANIFEST" no link.
    #[cfg(windows)]
    {
        attrs = attrs.windows_attributes(
            tauri_build::WindowsAttributes::new().app_manifest(include_str!("app.manifest")),
        );
    }

    tauri_build::try_build(attrs).expect("falha no tauri-build");
}
