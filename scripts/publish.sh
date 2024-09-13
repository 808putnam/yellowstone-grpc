#!/usr/bin/env bash

FULL_VERSION=$1

# TODO: Add back in when we can start using v prefixes
# if [[ ${FULL_VERSION} != v* ]]; then
#   echo "Version must start with 'v'"
#   exit 1
# fi

git tag -a ${FULL_VERSION}
git push origin ${FULL_VERSION}
