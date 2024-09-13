#!/usr/bin/env bash

FULL_VERSION=$1

# if [[ ${FULL_VERSION} != v* ]]; then
#   echo "Version must start with 'v'"
#   exit 1
# fi

git tag -a ${FULL_VERSION}
git push origin ${FULL_VERSION}
