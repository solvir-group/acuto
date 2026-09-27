
function ParseZedWorkspace {
    $metadata = cargo metadata --no-deps --offline | ConvertFrom-Json
    $env:ZED_WORKSPACE = $metadata.workspace_root
    # A release build sets the version from its tag; otherwise it is the app
    # crate's own. The crate is `acuto`: looking up `zed` found nothing, and the
    # installer was stamped with a blank version.
    if (-not $env:RELEASE_VERSION) {
        $env:RELEASE_VERSION = $metadata.packages | Where-Object { $_.name -eq "acuto" } | Select-Object -ExpandProperty version
    }
}
