# Named connections and secrets

This feature is included in v0.5.0. Upgrade the dashboard and apply its matching namespace permissions to use it. No existing credentials or workloads are migrated automatically.

## Connect an application

1. Open a project and choose **Connections & secrets → Add connection**.
2. Choose a name (for example, `Postgres`, `MySQL`, `Reporting Postgres`) and a type: PostgreSQL, MySQL, Redis, Kafka, or Custom / secrets. Names are unique within the project, ignoring case. Multiple connections of the same type are supported.
3. Add keys and values. Typical keys are `host`, `port`, `database`, `username`, `password`, or `url`. Custom keys can contain tokens or multiline certificates. These values are all stored in a Kubernetes Secret, not the account database.
4. Open an application’s **Environment → Connections & secrets**. Attach one or several project connections and map their keys to the exact environment names the application expects. For example, `Postgres.password → POSTGRES_PASSWORD` and `MySQL.url → MYSQL_URL`.
5. Attaching or changing mappings updates the pod template and requests a rollout. Check application status/logs for successful startup. Environment names must be unique across literal variables and all referenced connections. Existing external Secret/ConfigMap references are preserved.

A connection can be shared with several explicitly selected apps in its project. It may point at an in-cluster Service or an external database/broker. Use the Service’s actual service port, which can differ from the container port. Hostnames, credentials and client protocol settings must be appropriate for your backend. Kafka clients must reach all broker addresses advertised by the broker, not just the bootstrap address.

Connections store configuration only. They do not deploy databases or Kafka, create database users, configure TLS/SASL in the backend, test external endpoints, or provision backups. Databases deployed as applications still need the appropriate image configuration and persistent storage. Use the Custom type for secrets that are not database connections.

## Edit, rotate and remove

Existing values are never returned to the dashboard. Leave an existing value blank to keep it, or enter a replacement. Other unchanged keys are preserved. The API can explicitly set an empty value. Remove a key only after all consumers are detached.

The project connection list and editor show the KubeWeasel-managed apps in that project that use each connection. The application Environment screen shows its own status. If workload usage cannot be read, the UI reports it as unavailable rather than showing a misleading empty list.

Saving changed values marks affected apps **Restart required**. Unchanged values and name/type-only edits do not trigger a new data revision. Paused apps show that values apply on their next start. An explicit connection restart or the app’s ordinary Restart action records a non-sensitive revision marker in the pod template. A connection restart acknowledges all tracked connections reloaded by each affected app, while failed restart requests retain their warnings. The UI shows **Restart requested · rollout pending** until Kubernetes has observed the current workload generation and all desired replicas are updated and ready. A successful API request alone does not clear the rollout warning. State is persisted in Kubernetes metadata and survives a dashboard restart.

Revision markers are random identifiers, never hashes or copies of credential values. Tracking covers edits through KubeWeasel. Direct Secret edits outside KubeWeasel and unrelated image/config rollouts cannot prove which values a process loaded; use the explicit Restart action to acknowledge the current tracked version. The status describes rollout configuration, not whether database authentication succeeds. Older references without a marker conservatively require a restart to establish tracking.

Saving a replacement value does **not** restart apps automatically. Choose **Restart connected apps** when ready; the result reports requested and failed restarts separately. This targets KubeWeasel-managed apps in the same project. Apps using mounted files or external controllers may require their own reload procedure.

Changing a stored database password does not change the password of an already initialized database user. Coordinate the database-side password change with credential updates and application restarts. An existing PostgreSQL data directory, for example, is not reinitialized by changing an initialization environment variable.

Detach removes only this connection’s variable mappings from the selected app; the Secret and external database data remain. Deletion is blocked while namespace Pods or standard workload controllers still reference the connection, including init containers, volumes and projected Secrets. Old Pods, active ReplicaSets, Jobs, or CronJobs may still reference it and need review first. Inactive ReplicaSets owned by a Deployment are treated as rollout history; deleting a connection can make those old revisions unusable until their credentials are restored. Deletion never deletes databases, PVCs or registry/TLS Secrets. Kubernetes has no transaction spanning a Secret and every controller; changes made outside KubeWeasel can race with usage checks.

Detach connection variables before moving an app between projects. Delete a project’s unused connections before deleting its empty folder. Project names and connections stay scoped to the selected cluster and namespace.

## Permissions and protection

The matching Helm Role and remote namespace Role now grant `get`, `list`, `create`, `update`, and `delete` on namespace Secrets, plus list access to DaemonSets, ReplicaSets, Jobs and CronJobs for conservative usage checks. **Kubernetes RBAC cannot restrict these operations to a label:** these permissions grant the dashboard service account access to every Secret in its application namespace. Review this expansion before applying it to an existing installation. A dedicated application namespace limits the scope. Older namespace credentials can still use other capabilities but need updated permissions for connection management.

KubeWeasel’s owner authentication, request protection and selected-cluster dispatch cover these APIs. Application mutation requires an owned workload, matching project, current connection version and current workload version. Secret replacement/deletion uses Kubernetes resource-version/identity preconditions; errors from Secret operations are redacted. Values are never included in connection/config responses or persisted in browser storage. The browser necessarily holds newly entered values until they are submitted or the editor closes; use HTTPS for access outside a trusted private network.

Kubernetes Secrets are not automatically an encrypted vault. Their API representation uses base64; configure Kubernetes/K3s encryption at rest separately and protect cluster backups. Project folders organize attachments in the dashboard and are not network or Kubernetes security boundaries. No controller, administrator permission, public endpoint, or encryption setting is automatically installed by this feature.

## Checks

`cargo test --locked connections::` exercises authenticated multi-connection sharing, key rotation, stale-edit rejection, reference preservation and redacted responses. Mock Kubernetes API checks cover actual Secret requests and deletion safety. The ordinary demo uses memory only and resets on restart. Never use real credentials in tests.
