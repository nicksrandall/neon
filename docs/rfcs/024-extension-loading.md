# Supporting custom user Extensions

Created 2023-05-03

## Motivation

There are many extensions in the PostgreSQL ecosystem, and not all extensions
are of a quality that we can confidently support them. Additionally, our
current extension inclusion mechanism has several problems because we build all
extensions into the primary Compute image: We build the extensions every time
we build the compute image regardless of whether we actually need to rebuild
the image, and the inclusion of these extensions in the image adds a hard
dependency on all supported extensions - thus increasing the image size, and
with it the time it takes to download that image - increasing first start
latency.

This RFC proposes a dynamic loading mechanism that solves most of these
problems.

## Summary

`compute_ctl` is made responsible for loading extensions on-demand into
the container's file system for dynamically loaded extensions, and will also
make sure that the extensions in `shared_preload_libraries` are downloaded
before the compute node starts.

## Components

compute_ctl, PostgreSQL, neon (extension), Compute Host Node, Extension Store

## Requirements

Compute nodes with no extra extensions should not be negatively impacted by
the existence of support for many extensions.

Installing an extension into PostgreSQL should be easy.

Non-preloaded extensions shouldn't impact startup latency.

Uninstalled extensions shouldn't impact query latency.

A small latency penalty for dynamically loaded extensions is acceptable in
the first seconds of compute startup, but not in steady-state operations.

## Proposed implementation

### On-demand, JIT-loading of extensions

TLDR; we download extensions as soon as we need them, or when we have spare
time.

That means, we first download the extensions required to start the PostMaster
(`shared_preload_libraries` and their dependencies), then the libraries required
before a backend can start processing user input (`preload_libraries` and
dependencies), and then (with network limits applied) the remainder of the
configured extensions, with prioritization for installed extensions.

If PostgreSQL tries to load a library that is not yet fully on disk, it will
ask `compute_ctl` first if the extension has been downloaded yet, and will wait
for `compute_ctl` to finish downloading that extension. `compute_ctl` will
prioritize downloading that extension over other extensions that were not yet
requested.

#### Workflow

```mermaid
sequenceDiagram
    autonumber
    participant EX as External (control plane, ...)
    participant CTL as compute_ctl
    participant ST as extension store
    actor PG as PostgreSQL

    EX ->>+ CTL: Start compute with config X

    note over CTL: The configuration contains a list of all <br/>extensions available to that compute node, etc.

    par Optionally parallel or concurrent
        loop Available extensions
            CTL ->>+ ST: Download control file of extension
            activate CTL
            ST ->>- CTL: Finish downloading control file
            CTL ->>- CTL: Put control file in extensions directory
        end

        loop For each extension in shared_preload_libraries
            CTL ->>+ ST: Download extension's data
            activate CTL
            ST ->>- CTL: Finish downloading
            CTL ->>- CTL: Put extension's files in the right place
        end
    end

    CTL ->>+ PG: Start PostgreSQL

    note over CTL: PostgreSQL can now start accepting <br/>connections. However, users may still need to wait <br/>for preload_libraries extensions to get downloaded.

    par Load preload_libraries
        loop For each extension in preload_libraries
            CTL ->>+ ST: Download extension's data
            activate CTL
            ST ->>- CTL: Finish downloading
            CTL ->>- CTL: Put extension's files in the right place
        end
    end

    note over CTL: After this, connections don't have any hard <br/>waits for extension files left, except for those <br/>connections that override preload_libraries <br/>in their startup packet

    par PG's internal_load_library(library)
        alt Library is not yet loaded
            PG ->>+ CTL: Load library X
            CTL ->>+ ST: Download the extension that provides X
            ST ->>- CTL: Finish downloading
            CTL ->> CTL: Put extension's files in the right place
            CTL ->>- PG: Ready
        else Library is already loaded
            note over PG: No-op
        end
    and Download all remaining extensions
        loop Extension X
            CTL ->>+ ST: Download not-yet-downloaded extension X
            activate CTL
            ST ->>- CTL: Finish downloading
            CTL ->>- CTL: Put extension's files in the right place
        end
    end

    deactivate PG
    deactivate CTL
```

#### Summary

Pros:
 - Startup is only as slow as it takes to load all (shared_)preload_libraries
 - Supports BYO Extension

Cons:
 - O(sizeof(extensions)) IO requirement for loading all extensions.

### Alternative solutions

1. Allow users to add their extensions to the base image
   
   Pros:
    - Easy to deploy

   Cons:
    - Doesn't scale - first start size is dependent on image size;
    - All extensions are shared across all users: It doesn't allow users to
      bring their own restrictive-licensed extensions

2. Bring Your Own compute image
   
   Pros:
    - Still easy to deploy
    - User can bring own patched version of PostgreSQL

   Cons:
    - First start latency is O(sizeof(extensions image))
    - Warm instance pool for skipping pod schedule latency is not feasible with
      O(n) custom images
    - Support channels are difficult to manage

3. Download all user extensions in bulk on compute start
   
   Pros:
    - Easy to deploy
    - No startup latency issues for "clean" users.
    - Warm instance pool for skipping pod schedule latency is possible

   Cons:
    - Downloading all extensions in advance takes a lot of time, thus startup
      latency issues

4. Store user's extensions in persistent storage
   
   Pros:
    - Easy to deploy
    - No startup latency issues
    - Warm instance pool for skipping pod schedule latency is possible

   Cons:
    - EC2 instances have only limited number of attachments shared between EBS
      volumes, direct-attached NVMe drives, and ENIs.
    - Compute instance migration isn't trivially solved for EBS mounts (e.g.
      the device is unavailable whilst moving the mount between instances).
    - EBS can only mount on one instance at a time (except the expensive IO2
      device type).

5. Store user's extensions in network drive
   
   Pros:
    - Easy to deploy
    - Few startup latency issues
    - Warm instance pool for skipping pod schedule latency is possible

   Cons:
    - We'd need networked drives, and a lot of them, which would store many
      duplicate extensions.
    - **UNCHECKED:** Compute instance migration may not work nicely with
      networked IOs


### Idea extensions

The extension store does not have to be S3 directly, but could be a Node-local
caching service on top of S3. This would reduce the load on the network for
popular extensions.

## Extension Store implementation

Extension Store in our case is a private S3 bucket.
Extensions are stored as tarballs in the bucket. The tarball contains the extension's control file and all the files that the extension needs to run.

We may also store the control file separately from the tarball to speed up the extension loading.

`s3://<the-bucket>/extensions/ext-name/sha-256+1234abcd1234abcd1234abcd1234abcd/bundle.tar`

where `ext-name` is an extension name and `sha-256+1234abcd1234abcd1234abcd1234abcd` is a hash of a specific extension version tarball.

To ensure security, there is no direct access to the S3 bucket from compute node.

Control plane forms a list of extensions available to the compute node 
and forms a short-lived [pre-signed URL](https://docs.aws.amazon.com/AmazonS3/latest/userguide/ShareObjectPreSignedURL.html) 
for each extension that is available to the compute node.

so, `compute_ctl` receives spec in the following format

```
"extensions": [{
  "meta_format": 1,
  "extension_name": "postgis",
  "link": "https://<the-bucket>/extensions/sha-256+1234abcd1234abcd1234abcd1234abcd/bundle.tar?AWSAccessKeyId=1234abcd1234abcd1234abcd1234abcd&Expires=1234567890&Signature=1234abcd1234abcd1234abcd1234abcd",
  ...
}]
```

`compute_ctl` then downloads the extension from the link and unpacks it to the right place.

### How do we handle private extensions?

Private and public extensions are treated equally from the Extension Store perspective.
The only difference is that the private extensions are not listed in the user UI (managed by control plane).

### How to add new extension to the Extension Store?

Since we need to verify that the extension is compatible with the compute node and doesn't contain any malicious code, 
we need to review the extension before adding it to the Extension Store.

I do not expect that we will have a lot of extensions to review, so we can do it manually for now.

Some admin UI may be added later to automate this process.

The list of extensions available to a compute node is stored in the console database.

### How is the list of available extensions managed? 

We need to add new tables to the console database to store the list of available extensions, their versions and access rights.

something like this:

```
CREATE TABLE extensions (
    id SERIAL PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    version VARCHAR(255) NOT NULL,
    hash VARCHAR(255) NOT NULL, // this is the path to the extension in the Extension Store
    supported_postgres_versions integer[] NOT NULL, 
    is_public BOOLEAN NOT NULL, // public extensions are available to all users
    is_shared_preload BOOLEAN NOT NULL, // these extensions require postgres restart
    is_preload BOOLEAN NOT NULL,
    license VARCHAR(255) NOT NULL,
);

CREATE TABLE user_extensions (
    user_id INTEGER NOT NULL,
    extension_id INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users (id),
    FOREIGN KEY (extension_id) REFERENCES extensions (id)
);
```

When new extension is added to the Extension Store, we add a new record to the table and set permissions.
 
In UI, user may select the extensions that they want to use with their compute node.

NOTE: Extensions that require postgres restart will not be available until the next compute restart.
Also, currently user cannot force postgres restart. We should add this feature later.

For other extensions, we must communicate updates to `compute_ctl` and they will be downloaded in the background.

### How can user update the extension?

User can update the extension by selecting the new version of the extension in the UI.

### Alternatives

For extensions written on trusted languages we can also adopt
`dbdev` PostgreSQL Package Manager based on `pg_tle` by Supabase.
This will increase the amount supported extensions and decrease the amount of work required to support them.