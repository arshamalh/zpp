.PHONY: check test run-cloud run-gateways up down

check:            ## fmt + clippy + tests, exactly what CI runs
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace

test:
	cargo test --workspace

# Local run without Docker (needs a broker on localhost:1883, e.g. `brew install mosquitto`).
run-cloud:
	RUST_LOG=info,tower_http=warn cargo run --release -p cloud

run-gateways:
	SITE_ID=depot-utrecht PROFILE=depot SEED=7 DROP_ACK_PCT=20 cargo run --release -p gateway-sim & \
	SITE_ID=campus-eindhoven PROFILE=campus SEED=42 cargo run --release -p gateway-sim & \
	SITE_ID=cold-tilburg PROFILE=cold-storage SEED=99 cargo run --release -p gateway-sim & \
	wait

up:               ## full stack in Docker, then open http://localhost:8080
	docker compose up --build -d

down:
	docker compose down -v

# ----------------------------------------------------------------------------
# Boteland cluster (k3s): https://peakpilot.boteland.com, namespace `peakpilot`
# ----------------------------------------------------------------------------
# Every target below talks ONLY to the boteland-k3s context and ONLY touches the
# peakpilot namespace (plus the peakpilot image in the node's containerd).
# Prereqs on your Mac: kubectl, docker with buildx, dig, and `ssh $(HOST_SSH)`
# working as root (the same ssh alias the boteland repo uses).
#
#   make deploy     build -> ship image to the node -> roll out   (re-run to update)
#   make status     pods, ingress, certificate, public health check
#   make logs       LOGS=cloud|gw-depot|gw-campus|gw-cold|mosquitto
#   make undeploy   delete the namespace and everything in it

NS             ?= peakpilot
DOMAIN         ?= peakpilot.boteland.com
HOST_IP        ?= 2.58.32.108
HOST_SSH       ?= boteland
# The admin kubeconfig the boteland repo writes with `make host-kubeconfig`.
K8S_KUBECONFIG ?= $(HOME)/projects/boteland/.kube/boteland-k3s.yaml
K8S_CONTEXT    ?= boteland-k3s
# The node's architecture (the image is cross-built from Apple silicon).
PLATFORM       ?= linux/amd64
# Commit hash, plus a hash of uncommitted changes, so every code change rolls the
# pods and the same working tree always maps to the same tag.
TAG            := $(shell git rev-parse --short HEAD 2>/dev/null || echo dev)$(shell git diff --quiet HEAD 2>/dev/null || echo -dirty-$$(git diff HEAD | git hash-object --stdin | cut -c1-7))
IMAGE          := peakpilot:$(TAG)
KUBECTL         = kubectl --kubeconfig $(K8S_KUBECONFIG) --context $(K8S_CONTEXT)

.PHONY: deploy cluster-check dns-check image image-import image-prune db-secret render apply status logs cert-status open undeploy

deploy: cluster-check dns-check image image-import db-secret apply image-prune
	@echo "Deployed $(IMAGE) -> https://$(DOMAIN)"

cluster-check:
	@test -f "$(K8S_KUBECONFIG)" || { echo "ERROR: no kubeconfig at $(K8S_KUBECONFIG). Run 'make host-kubeconfig' in the boteland repo or pass K8S_KUBECONFIG=..."; exit 1; }
	@$(KUBECTL) get --raw /readyz >/dev/null || { echo "ERROR: cluster '$(K8S_CONTEXT)' is not reachable"; exit 1; }
	@$(KUBECTL) get clusterissuer letsencrypt-prod >/dev/null 2>&1 || { echo "ERROR: ClusterIssuer letsencrypt-prod missing (boteland repo: make cert-manager cert)"; exit 1; }
	@ssh -o BatchMode=yes -o ConnectTimeout=10 $(HOST_SSH) 'command -v k3s >/dev/null' || { echo "ERROR: 'ssh $(HOST_SSH)' failed or k3s not found on the node"; exit 1; }
	@echo "cluster $(K8S_CONTEXT): OK"

# Let's Encrypt validates over HTTP, so the name must point at the node first.
dns-check:
	@ip=$$(dig +short $(DOMAIN) A | tail -1); \
	if [ "$$ip" = "$(HOST_IP)" ]; then echo "$(DOMAIN) -> $$ip: OK"; \
	else echo "ERROR: $(DOMAIN) resolves to '$$ip', expected $(HOST_IP). Add a DNS A record (or CNAME to boteland.com) first."; exit 1; fi

# First build is slow (Rust under amd64 emulation); later builds reuse the
# cached dependency layer and only recompile our three crates.
image:
	docker buildx build --platform $(PLATFORM) --provenance=false --sbom=false --load -t $(IMAGE) .

# Stream the image straight into the node's containerd ("k8s.io" is the
# namespace the kubelet reads). No registry: nothing to log in to, and nothing
# left in boteland's Zot registry (or its push webhook) once this is removed.
image-import:
	docker save $(IMAGE) | gzip -1 | ssh $(HOST_SSH) 'gunzip | k3s ctr -n k8s.io images import -'

# Drop older peakpilot images from the node (disk there is tight).
image-prune:
	@ssh $(HOST_SSH) "k3s ctr -n k8s.io images ls -q | grep '^docker.io/library/peakpilot:' | grep -v ':$(TAG)\$$' | xargs -r k3s ctr -n k8s.io images rm" >/dev/null

# Random DB password, created once and kept on re-deploys (regenerating it
# would lock the cloud out of the existing database).
db-secret:
	@$(KUBECTL) apply -f deploy/k8s/namespace.yaml >/dev/null
	@if $(KUBECTL) -n $(NS) get secret postgres >/dev/null 2>&1; then \
		echo "secret/postgres exists, keeping it"; \
	else \
		pw=$$(openssl rand -hex 24); \
		$(KUBECTL) -n $(NS) create secret generic postgres \
			--from-literal=POSTGRES_USER=peakpilot \
			--from-literal=POSTGRES_PASSWORD=$$pw \
			--from-literal=POSTGRES_DB=peakpilot \
			--from-literal=DATABASE_URL=postgres://peakpilot:$$pw@postgres/peakpilot >/dev/null \
		&& echo "created secret/postgres with a random password"; \
	fi

# Print the exact manifests `apply` sends (handy for review).
render:
	@$(KUBECTL) kustomize deploy/boteland | sed 's|peakpilot:__TAG__|$(IMAGE)|g'

apply:
	$(KUBECTL) kustomize deploy/boteland | sed 's|peakpilot:__TAG__|$(IMAGE)|g' | $(KUBECTL) apply -f -
	$(KUBECTL) -n $(NS) rollout status statefulset/postgres --timeout=180s
	$(KUBECTL) -n $(NS) rollout status deployment/cloud --timeout=180s
	@for d in mosquitto gw-depot gw-campus gw-cold; do \
		$(KUBECTL) -n $(NS) rollout status deployment/$$d --timeout=120s || exit 1; done

status:
	$(KUBECTL) -n $(NS) get pods,svc,ingress,certificate,pvc
	@curl -fsS -o /dev/null -w "https://$(DOMAIN)/readyz -> %{http_code}\n" https://$(DOMAIN)/readyz || echo "https://$(DOMAIN) not answering yet (see 'make cert-status')"

logs:
	$(KUBECTL) -n $(NS) logs -f deployment/$(or $(LOGS),cloud)

cert-status:
	$(KUBECTL) -n $(NS) describe certificate peakpilot-tls

open:
	open https://$(DOMAIN)

# The Postgres volume uses k3s's default `local-path` class (reclaim: Delete),
# so deleting the namespace removes the data too. Nothing outside it to clean.
undeploy:
	@printf "Delete namespace '$(NS)' on $(K8S_CONTEXT), including its database? [y/N] "; read a; [ "$$a" = y ] || { echo aborted; exit 1; }
	$(KUBECTL) delete namespace $(NS) --wait
	-@ssh $(HOST_SSH) "k3s ctr -n k8s.io images ls -q | grep '^docker.io/library/peakpilot:' | xargs -r k3s ctr -n k8s.io images rm" >/dev/null
	@echo "Removed. You can also delete the DNS record for $(DOMAIN)."
