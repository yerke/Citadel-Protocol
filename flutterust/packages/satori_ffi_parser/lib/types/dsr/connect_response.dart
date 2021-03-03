import 'package:optional/optional.dart';
import 'package:satori_ffi_parser/types/domain_specific_response.dart';
import 'package:satori_ffi_parser/types/domain_specific_response_type.dart';
import 'package:satori_ffi_parser/types/standard_ticket.dart';

import '../u64.dart';

class ConnectResponse extends DomainSpecificResponse {
  final String message;
  final StandardTicket ticket;
  final u64 implicated_cid;
  final bool success;
  Optional<String> username = Optional.empty();

  ConnectResponse._(this.success, this.ticket, this.implicated_cid, this.message);

  @override
  Optional<String> getMessage() {
    return Optional.of(this.message);
  }

  @override
  Optional<StandardTicket> getTicket() {
    return Optional.of(this.ticket);
  }

  @override
  DomainSpecificResponseType getType() {
    return DomainSpecificResponseType.Connect;
  }

  static Optional<DomainSpecificResponse> tryFrom(Map<String, dynamic> infoNode) {
    bool success = infoNode.containsKey("Success");
    List<dynamic> leaf = infoNode[success ? "Success" : "Failure"];
    if (leaf.length != 3) {
      return Optional.empty();
    }

    var ticket = StandardTicket.tryFrom(leaf[0]);
    var implicated_cid = u64.tryFrom(leaf[1]);
    String message = leaf[2];

    return ticket.isPresent && implicated_cid.isPresent ? Optional.of(ConnectResponse._(success, ticket.value, implicated_cid.value, message)) : Optional.empty();
  }

  void attachUsername(String username) {
    this.username = Optional.of(username);
  }

  Optional<String> getAttachedUsername() {
    return this.username;
  }

  @override
  bool isFcm() {
    return false;
  }
}