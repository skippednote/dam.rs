# The nightly conformance runner's AWS identity

`nightly-aws.yml` is the only place real Glacier semantics are exercised. A scheduled run cannot
borrow an SSO session, so it needs a long-lived IAM principal — which is the one thing in this
project's infrastructure that has to exist outside the code.

This module creates the user and its policy. **It deliberately does not create the access key.**

## Why the key is not in here

`aws_iam_access_key` puts the secret in Terraform state. State is a file people commit, share, and
push to buckets, and a secret in it is a secret in all of those places — for as long as the state
exists, which is longer than the key. The usual workarounds (encrypt the state, mark it sensitive)
protect the display and not the storage.

So the key is created once, by a person, and pasted straight into GitHub's secret store where it is
write-only. Nothing that holds it also holds a copy.

## Apply

```sh
terraform init
terraform apply -var="bucket=your-conformance-bucket"
```

Then create the key and set the secrets — `gh secret set` prompts and reads hidden, so the value
never reaches your shell history:

```sh
aws iam create-access-key --user-name damrs-nightly-conformance
gh secret set AWS_ACCESS_KEY_ID
gh secret set AWS_SECRET_ACCESS_KEY
gh secret set DAMRS_TEST_BUCKET
```

Finally, uncomment the `schedule:` trigger in `.github/workflows/nightly-aws.yml`. It is commented
precisely so that an unconfigured nightly cannot fail every night in a public repository, which is a
red run for a reason no reader can distinguish from "the archival tests fail".

## Rotation

The key is the only long-lived credential this project needs. `aws iam create-access-key` a second
one, set the secrets, then delete the first — the user tolerates two keys so rotation needs no
downtime.
