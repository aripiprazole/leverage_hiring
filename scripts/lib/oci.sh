#!/usr/bin/env bash

parse_oci_reference() {
    local image="$1"

    [[ "$image" == *:* ]] ||
        usage_error "expected a Docker Hub image like alpine:latest, got: $image"

    OCI_IMAGE_NAME="${image%:*}"
    OCI_IMAGE_TAG="${image##*:}"

    [[ "$OCI_IMAGE_NAME" =~ ^[a-z0-9]+([._-][a-z0-9]+)*(/[a-z0-9]+([._-][a-z0-9]+)*)*$ ]] ||
        usage_error "invalid Docker Hub image name: $OCI_IMAGE_NAME"
    [[ "$OCI_IMAGE_TAG" =~ ^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$ ]] ||
        usage_error "invalid Docker Hub image tag: $OCI_IMAGE_TAG"

    OCI_REPOSITORY="$OCI_IMAGE_NAME"
    if [[ "$OCI_REPOSITORY" != */* ]]; then
        OCI_REPOSITORY="library/$OCI_REPOSITORY"
    fi
}

docker_public_token() {
    local repository="$1"
    local token

    if ! token="$(
        curl \
            --fail \
            --silent \
            --show-error \
            --get \
            --data-urlencode 'service=registry.docker.io' \
            --data-urlencode "scope=repository:$repository:pull" \
            'https://auth.docker.io/token' |
            jq -er '(.token // .access_token) | select(type == "string" and length > 0)'
    )"; then
        die "failed to fetch a Docker Hub public token for $repository"
    fi

    printf '%s\n' "$token"
}

oci_pull_payload() {
    local token
    token="$(docker_public_token "$OCI_REPOSITORY")"
    jq -nc \
        --arg name "$OCI_IMAGE_NAME" \
        --arg tag "$OCI_IMAGE_TAG" \
        --arg token "$token" \
        '{name: $name, tag: $tag, token: $token}'
}

oci_reference_payload() {
    jq -nc \
        --arg name "$OCI_IMAGE_NAME" \
        --arg tag "$OCI_IMAGE_TAG" \
        '{name: $name, tag: $tag}'
}
