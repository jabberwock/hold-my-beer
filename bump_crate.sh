#!/bin/bash

# Function to update version in Cargo.toml
update_version() {
local file=$1
local new_version=$2

# Check if file exists
if [[ ! -f $file ]]; then
echo "File not found: $file"
return 1
fi

# Update the version using sed
sed -i '' "s/^version = \".*\"/version = \"$new_version\"/" "$file"

echo "Updated $file to version $new_version"
}

# Main script execution
if [[ $# -ne 2 ]]; then
echo "Usage: $0 <collab-server-version> <collab-cli-version>"
exit 1
fi

collab_server_version=$1
collab_cli_version=$2

# Update versions in Cargo.toml files
update_version "collab-server/Cargo.toml" "$collab_server_version"
update_version "collab-cli/Cargo.toml" "$collab_cli_version"

echo "Version update complete."
