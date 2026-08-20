#!/usr/bin/env bash

set -euo pipefail

require_env() {
  local name
  for name in "$@"; do
    if [[ -z "${!name:-}" ]]; then
      echo "Missing required environment variable: $name" >&2
      exit 1
    fi
  done
}

inspect_digest() {
  local ref=$1
  local error_file="$RUNNER_TEMP/imagetools-inspect-error"
  local manifest
  if manifest=$(docker buildx imagetools inspect "$ref" --format '{{json .Manifest}}' 2>"$error_file"); then
    jq -er '.digest' <<<"$manifest"
    return
  fi
  if grep -Eqi 'manifest unknown|not found|no such manifest' "$error_file"; then
    printf ''
    return
  fi
  cat "$error_file" >&2
  return 1
}

inspect_version() {
  local ref=$1
  local error_file="$RUNNER_TEMP/imagetools-version-error"
  local child_digest
  local image_ref=$ref
  local labels
  local manifest
  local version
  if ! manifest=$(docker buildx imagetools inspect "$ref" --raw 2>"$error_file"); then
    cat "$error_file" >&2
    return 1
  fi
  version=$(jq -r '.annotations."org.opencontainers.image.version" // empty' <<<"$manifest")
  if [[ -n "$version" ]]; then
    printf '%s' "$version"
    return
  fi
  child_digest=$(jq -r '
    [.manifests[]? | select(.platform.os == "linux" and .platform.architecture == "amd64")][0].digest // empty
  ' <<<"$manifest")
  if [[ -z "$child_digest" ]]; then
    child_digest=$(jq -r '
      [.manifests[]? | select(.platform.os == "linux" and .platform.architecture == "arm64")][0].digest // empty
    ' <<<"$manifest")
  fi
  if [[ -n "$child_digest" ]]; then
    image_ref="${ref%:*}@$child_digest"
  fi
  if ! labels=$(docker buildx imagetools inspect "$image_ref" --format '{{json .Image.Config.Labels}}' 2>"$error_file"); then
    cat "$error_file" >&2
    return 1
  fi
  jq -er '."org.opencontainers.image.version"' <<<"$labels"
}

arch_digests_for() {
  local image=$1
  if [[ "$image" == "$BROWSER_IMAGE" ]]; then
    printf '%s %s' "$BROWSER_DIGEST_AMD64" "$BROWSER_DIGEST_ARM64"
  else
    printf '%s %s' "$MAIN_DIGEST_AMD64" "$MAIN_DIGEST_ARM64"
  fi
}

expected_arch_digests_for() {
  local image=$1
  if [[ "$image" == "$BROWSER_IMAGE" ]]; then
    printf '%s %s' "$BROWSER_PLATFORM_AMD64" "$BROWSER_PLATFORM_ARM64"
  else
    printf '%s %s' "$MAIN_PLATFORM_AMD64" "$MAIN_PLATFORM_ARM64"
  fi
}

resolve_source_platform_digest() {
  local image=$1
  local outer_digest=$2
  local architecture=$3
  local ref="$image@$outer_digest"
  local actual_architecture
  local actual_os
  local image_config
  local leaf_digest
  local match_count
  local media_type
  local raw
  local error_file="$RUNNER_TEMP/imagetools-source-$architecture-error"

  if ! raw=$(docker buildx imagetools inspect "$ref" --raw 2>"$error_file"); then
    cat "$error_file" >&2
    return 1
  fi
  if ! media_type=$(jq -er '.mediaType' <<<"$raw"); then
    echo "Source $ref has no readable media type." >&2
    return 1
  fi

  case "$media_type" in
    application/vnd.oci.image.index.v1+json|application/vnd.docker.distribution.manifest.list.v2+json)
      if ! match_count=$(jq -er --arg architecture "$architecture" \
        '[.manifests[]? | select(.platform.os == "linux" and .platform.architecture == $architecture)] | length' \
        <<<"$raw"); then
        echo "Could not inspect the linux/$architecture children in source index $ref." >&2
        return 1
      fi
      if [[ "$match_count" != 1 ]]; then
        echo "Source index $ref must contain exactly one linux/$architecture image manifest; found $match_count." >&2
        return 1
      fi
      if ! leaf_digest=$(jq -er --arg architecture "$architecture" \
        '[.manifests[]? | select(.platform.os == "linux" and .platform.architecture == $architecture)][0].digest' \
        <<<"$raw"); then
        echo "Could not read the linux/$architecture image digest from source index $ref." >&2
        return 1
      fi
      ;;
    application/vnd.oci.image.manifest.v1+json|application/vnd.docker.distribution.manifest.v2+json)
      if ! image_config=$(docker buildx imagetools inspect "$ref" --format '{{json .Image}}' 2>"$error_file"); then
        cat "$error_file" >&2
        return 1
      fi
      if ! actual_os=$(jq -er '.os' <<<"$image_config") \
        || ! actual_architecture=$(jq -er '.architecture' <<<"$image_config"); then
        echo "Direct image manifest $ref has no readable platform configuration." >&2
        return 1
      fi
      if [[ "$actual_os" != linux || "$actual_architecture" != "$architecture" ]]; then
        echo "Direct image manifest $ref is $actual_os/$actual_architecture, expected linux/$architecture." >&2
        return 1
      fi
      leaf_digest=$outer_digest
      ;;
    *)
      echo "Source $ref has unsupported media type '$media_type'." >&2
      return 1
      ;;
  esac

  if [[ ! "$leaf_digest" =~ ^sha256:[0-9a-f]{64}$ ]]; then
    echo "Source $ref resolved an invalid linux/$architecture digest: ${leaf_digest:-<empty>}." >&2
    return 1
  fi
  printf '%s' "$leaf_digest"
}

resolve_source_platform_digests() {
  if ! BROWSER_PLATFORM_AMD64=$(resolve_source_platform_digest \
    "$BROWSER_IMAGE" "$BROWSER_DIGEST_AMD64" amd64); then
    return 1
  fi
  if ! BROWSER_PLATFORM_ARM64=$(resolve_source_platform_digest \
    "$BROWSER_IMAGE" "$BROWSER_DIGEST_ARM64" arm64); then
    return 1
  fi
  if ! MAIN_PLATFORM_AMD64=$(resolve_source_platform_digest \
    "$MAIN_IMAGE" "$MAIN_DIGEST_AMD64" amd64); then
    return 1
  fi
  if ! MAIN_PLATFORM_ARM64=$(resolve_source_platform_digest \
    "$MAIN_IMAGE" "$MAIN_DIGEST_ARM64" arm64); then
    return 1
  fi
}

manifest_digest() {
  local manifest=$1
  local checksum
  if ! checksum=$(printf '%s' "$manifest" | sha256sum); then
    echo "Failed to hash the dry-run candidate index." >&2
    return 1
  fi
  checksum=${checksum%% *}
  if [[ ! "$checksum" =~ ^[0-9a-f]{64}$ ]]; then
    echo "Dry-run candidate index produced an invalid SHA-256 checksum: ${checksum:-<empty>}" >&2
    return 1
  fi
  printf 'sha256:%s' "$checksum"
}

verify_index_json() {
  local image=$1
  local ref=$2
  local raw=$3
  local amd64_digest
  local annotation
  local arch_manifests
  local arm64_digest
  local expected_manifests
  local media_type
  local revision
  read -r amd64_digest arm64_digest <<<"$(expected_arch_digests_for "$image")"
  jq -e . >/dev/null <<<"$raw"
  arch_manifests=$(jq -r '[.manifests[]? | select(.platform.os == "linux") | .digest] | sort | join(",")' <<<"$raw")
  expected_manifests=$(printf '%s\n%s\n' "$amd64_digest" "$arm64_digest" | LC_ALL=C sort | paste -sd, -)
  if [[ "$arch_manifests" != "$expected_manifests" ]]; then
    echo "Index $ref does not merge exactly the candidate architecture manifests: $arch_manifests." >&2
    return 1
  fi
  media_type=$(jq -r '.mediaType // empty' <<<"$raw")
  if [[ "$media_type" != "application/vnd.oci.image.index.v1+json" ]]; then
    echo "Index $ref has media type '$media_type'; expected an OCI image index." >&2
    return 1
  fi
  annotation=$(jq -r '.annotations."org.opencontainers.image.version" // empty' <<<"$raw")
  if [[ "$annotation" != "$CANDIDATE_VERSION" ]]; then
    echo "Index $ref carries version annotation '$annotation' instead of '$CANDIDATE_VERSION'." >&2
    return 1
  fi
  revision=$(jq -r '.annotations."org.opencontainers.image.revision" // empty' <<<"$raw")
  if [[ "$revision" != "$FULL_SHA" ]]; then
    echo "Index $ref carries revision annotation '$revision' instead of '$FULL_SHA'." >&2
    return 1
  fi
}

candidate_manifest_for() {
  local image=$1
  local amd64_digest
  local arm64_digest
  read -r amd64_digest arm64_digest <<<"$(arch_digests_for "$image")"
  docker buildx imagetools create \
    --dry-run \
    --progress quiet \
    --annotation "index:org.opencontainers.image.version=$CANDIDATE_VERSION" \
    --annotation "index:org.opencontainers.image.revision=$FULL_SHA" \
    "$image@$amd64_digest" "$image@$arm64_digest"
}

verify_published_digest() {
  local ref=$1
  local candidate_digest=$2
  local actual
  actual=$(inspect_digest "$ref")
  if [[ "$actual" != "$candidate_digest" ]]; then
    echo "Published digest mismatch for $ref: $actual != $candidate_digest" >&2
    return 1
  fi
}

verify_remote_candidate_index() {
  local image=$1
  local ref=$2
  local candidate_digest=$3
  local raw
  if ! raw=$(docker buildx imagetools inspect "$ref" --raw 2>"$RUNNER_TEMP/imagetools-index-error"); then
    cat "$RUNNER_TEMP/imagetools-index-error" >&2
    return 1
  fi
  verify_index_json "$image" "$ref" "$raw"
  verify_published_digest "$ref" "$candidate_digest"
}

check_immutable() {
  local image=$1
  local tag=$2
  local candidate_digest=$3
  local ref="$image:$tag"
  local existing
  existing=$(inspect_digest "$ref") || return 1
  node scripts/release-policy.mjs immutable-tag \
    --tag "$tag" \
    --candidate-digest "$candidate_digest" \
    --existing-digest "$existing"
}

publish_candidate_index() {
  local image=$1
  local ref=$2
  local amd64_digest
  local arm64_digest
  read -r amd64_digest arm64_digest <<<"$(arch_digests_for "$image")"
  docker buildx imagetools create \
    --progress quiet \
    --tag "$ref" \
    --annotation "index:org.opencontainers.image.version=$CANDIDATE_VERSION" \
    --annotation "index:org.opencontainers.image.revision=$FULL_SHA" \
    "$image@$amd64_digest" "$image@$arm64_digest"
}

publish_immutable_phase() {
  require_env \
    BROWSER_DIGEST_AMD64 BROWSER_DIGEST_ARM64 BROWSER_IMAGE CANDIDATE_VERSION FULL_SHA \
    GITHUB_OUTPUT MAIN_DIGEST_AMD64 MAIN_DIGEST_ARM64 MAIN_IMAGE RUNNER_TEMP SHORT_SHA

  local arch_digest_var
  for arch_digest_var in \
    MAIN_DIGEST_AMD64 MAIN_DIGEST_ARM64 BROWSER_DIGEST_AMD64 BROWSER_DIGEST_ARM64; do
    if [[ ! "${!arch_digest_var}" =~ ^sha256:[0-9a-f]{64}$ ]]; then
      echo "Invalid $arch_digest_var; both architecture legs must publish content digests." >&2
      exit 1
    fi
  done

  # BuildKit provenance makes a single-architecture output an outer OCI index
  # containing one platform image plus unknown-platform attestation manifests.
  # Keep the outer digests as create sources, but verify the combined index
  # against each source's unique platform image leaf.
  resolve_source_platform_digests

  local browser_manifest
  local main_manifest
  local browser_digest
  local main_digest
  # Dry-run assembly is the only candidate-digest source. It reads the pushed
  # architecture manifests but does not create a tag or other registry object.
  browser_manifest=$(candidate_manifest_for "$BROWSER_IMAGE")
  main_manifest=$(candidate_manifest_for "$MAIN_IMAGE")
  verify_index_json "$BROWSER_IMAGE" "browser candidate dry-run" "$browser_manifest"
  verify_index_json "$MAIN_IMAGE" "main candidate dry-run" "$main_manifest"
  browser_digest=$(manifest_digest "$browser_manifest")
  main_digest=$(manifest_digest "$main_manifest")

  declare -A immutable_decisions
  local candidate_digest
  local image
  local image_and_digest
  local key
  local tag
  # Complete all four immutable-tag decisions before the first user-visible tag write.
  for image_and_digest in \
    "$MAIN_IMAGE $main_digest" \
    "$BROWSER_IMAGE $browser_digest"; do
    read -r image candidate_digest <<<"$image_and_digest"
    for tag in "$CANDIDATE_VERSION" "sha-$SHORT_SHA"; do
      key="$image:$tag"
      immutable_decisions["$key"]=$(check_immutable "$image" "$tag" "$candidate_digest")
    done
  done

  publish_immutable_image_tags() {
    local image=$1
    local candidate_digest=$2
    local ref
    local decision
    local immutable_tag
    for immutable_tag in "$CANDIDATE_VERSION" "sha-$SHORT_SHA"; do
      ref="$image:$immutable_tag"
      decision=${immutable_decisions["$ref"]}
      if [[ "$decision" == create ]]; then
        publish_candidate_index "$image" "$ref"
      else
        echo "Immutable tag $ref already resolves to the candidate digest; leaving it unchanged."
      fi
      verify_remote_candidate_index "$image" "$ref" "$candidate_digest"
    done
  }

  # Publish the dependency first, then the main image, and verify both exact tags.
  publish_immutable_image_tags "$BROWSER_IMAGE" "$browser_digest"
  publish_immutable_image_tags "$MAIN_IMAGE" "$main_digest"
  verify_published_digest "$BROWSER_IMAGE:$CANDIDATE_VERSION" "$browser_digest"
  verify_published_digest "$MAIN_IMAGE:$CANDIDATE_VERSION" "$main_digest"
  verify_published_digest "$BROWSER_IMAGE:sha-$SHORT_SHA" "$browser_digest"
  verify_published_digest "$MAIN_IMAGE:sha-$SHORT_SHA" "$main_digest"

  echo "main_digest=$main_digest" >>"$GITHUB_OUTPUT"
  echo "browser_digest=$browser_digest" >>"$GITHUB_OUTPUT"
}

advance_moving_phase() {
  require_env \
    BROWSER_DIGEST BROWSER_DIGEST_AMD64 BROWSER_DIGEST_ARM64 BROWSER_IMAGE \
    CANDIDATE_VERSION FULL_SHA MAIN_DIGEST MAIN_DIGEST_AMD64 MAIN_DIGEST_ARM64 \
    MAIN_IMAGE PUBLISH_LATEST RUNNER_TEMP SHORT_SHA STABLE

  if [[ "$STABLE" != true ]]; then
    echo "Prerelease $CANDIDATE_VERSION has no moving container channels."
    return
  fi
  require_env MINOR_CHANNEL
  resolve_source_platform_digests

  # This is a fresh post-attestation read. Moving-channel decisions never reuse
  # registry state captured before immutable publication or public verification.
  verify_remote_candidate_index "$BROWSER_IMAGE" "$BROWSER_IMAGE:$CANDIDATE_VERSION" "$BROWSER_DIGEST"
  verify_remote_candidate_index "$MAIN_IMAGE" "$MAIN_IMAGE:$CANDIDATE_VERSION" "$MAIN_DIGEST"
  verify_published_digest "$BROWSER_IMAGE:sha-$SHORT_SHA" "$BROWSER_DIGEST"
  verify_published_digest "$MAIN_IMAGE:sha-$SHORT_SHA" "$MAIN_DIGEST"

  declare -A moving_decisions
  declare -A moving_current_versions
  declare -A moving_existing_digests
  declare -A moving_expected_versions
  declare -A expected_digests

  preflight_moving_pair() {
    local tag=$1
    local main_ref="$MAIN_IMAGE:$tag"
    local browser_ref="$BROWSER_IMAGE:$tag"
    local main_existing
    local browser_existing
    local main_current=
    local browser_current=
    local decision
    local main_advance
    local browser_advance
    local version
    main_existing=$(inspect_digest "$main_ref")
    browser_existing=$(inspect_digest "$browser_ref")
    if [[ -n "$main_existing" ]]; then
      main_current=$(inspect_version "$main_ref")
    fi
    if [[ -n "$browser_existing" ]]; then
      browser_current=$(inspect_version "$browser_ref")
    fi
    decision=$(node scripts/release-policy.mjs paired-channel \
      --candidate "$CANDIDATE_VERSION" \
      --main-current "$main_current" \
      --browser-current "$browser_current")
    main_advance=$(jq -er '.mainAdvance' <<<"$decision")
    browser_advance=$(jq -er '.browserAdvance' <<<"$decision")
    version=$(jq -er '.version' <<<"$decision")
    moving_decisions["$main_ref"]=$main_advance
    moving_decisions["$browser_ref"]=$browser_advance
    moving_current_versions["$main_ref"]=$main_current
    moving_current_versions["$browser_ref"]=$browser_current
    moving_existing_digests["$main_ref"]=$main_existing
    moving_existing_digests["$browser_ref"]=$browser_existing
    moving_expected_versions["$tag"]=$version
  }

  resolve_moving_expected_digests() {
    local tag=$1
    local main_ref="$MAIN_IMAGE:$tag"
    local browser_ref="$BROWSER_IMAGE:$tag"
    expected_digests["$main_ref"]=$([[ "${moving_decisions["$main_ref"]}" == true ]] && printf '%s' "$MAIN_DIGEST" || printf '%s' "${moving_existing_digests["$main_ref"]}")
    expected_digests["$browser_ref"]=$([[ "${moving_decisions["$browser_ref"]}" == true ]] && printf '%s' "$BROWSER_DIGEST" || printf '%s' "${moving_existing_digests["$browser_ref"]}")
  }

  preflight_moving_pair "$MINOR_CHANNEL"
  if [[ "$PUBLISH_LATEST" == true ]]; then
    preflight_moving_pair latest
  fi
  resolve_moving_expected_digests "$MINOR_CHANNEL"
  if [[ "$PUBLISH_LATEST" == true ]]; then
    resolve_moving_expected_digests latest
  fi

  publish_moving() {
    local image=$1
    local tag=$2
    local candidate_digest=$3
    local ref="$image:$tag"
    local advance=${moving_decisions["$ref"]}
    local current_version=${moving_current_versions["$ref"]}
    if [[ "$advance" == true ]]; then
      # A single index source is copied byte-for-byte, so the moving tag must
      # resolve to the already published and attested candidate index digest.
      docker buildx imagetools create --progress quiet --tag "$ref" "$image@$candidate_digest"
      verify_published_digest "$ref" "$candidate_digest"
      echo "Advanced $ref from ${current_version:-none} to $CANDIDATE_VERSION."
    else
      echo "Kept $ref at newer or equal version $current_version."
    fi
  }

  verify_paired_moving_tag() {
    local tag=$1
    local main_ref="$MAIN_IMAGE:$tag"
    local browser_ref="$BROWSER_IMAGE:$tag"
    local expected_version=${moving_expected_versions["$tag"]}
    local main_version
    local browser_version
    verify_published_digest "$main_ref" "${expected_digests["$main_ref"]}"
    verify_published_digest "$browser_ref" "${expected_digests["$browser_ref"]}"
    main_version=$(inspect_version "$main_ref")
    browser_version=$(inspect_version "$browser_ref")
    if [[ "$main_version" != "$expected_version" || "$browser_version" != "$expected_version" ]]; then
      echo "Paired channel $tag is split after publication: main=$main_version, browser=$browser_version, expected=$expected_version." >&2
      return 1
    fi
  }

  publish_moving_pair() {
    local tag=$1
    # Publish the sidecar first so the matching dependency exists before the main image moves.
    publish_moving "$BROWSER_IMAGE" "$tag" "$BROWSER_DIGEST"
    publish_moving "$MAIN_IMAGE" "$tag" "$MAIN_DIGEST"
    verify_paired_moving_tag "$tag"
  }

  publish_moving_pair "$MINOR_CHANNEL"
  if [[ "$PUBLISH_LATEST" == true ]]; then
    publish_moving_pair latest
  fi
}

case "${1:-}" in
  publish-immutable)
    publish_immutable_phase
    ;;
  advance-moving)
    advance_moving_phase
    ;;
  *)
    echo "Usage: $0 publish-immutable|advance-moving" >&2
    exit 2
    ;;
esac
